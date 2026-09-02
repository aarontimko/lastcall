//! Scenario suite C — upstream changes (docs/spec/01-scenarios.md §C): grouping by author
//! and the `mixed` badge.

mod common;

use common::Fresh;
use lastcall_engine::headstate::{self, InProgress};
use lastcall_engine::ops::{NoFault, Rendered};
use lastcall_engine::scan::Annotation;
use lastcall_testkit::assert_pile;

/// Accept every upstream group the way the UI's "accept group" does.
fn accept_groups(s: &mut Fresh) {
    let pile = s.scan();
    for group in pile.groups() {
        let rendered: Vec<Rendered> = group
            .paths
            .iter()
            .map(|p| Rendered::of(pile.row(p).unwrap()))
            .collect();
        let out = s
            .engine
            .ops(&s.root)
            .unwrap()
            .accept_group(&rendered, &NoFault)
            .unwrap();
        assert!(out.ok(), "{out:?}");
    }
}

#[test]
fn scenario_c1_c2_fetch_then_ff_pull() {
    let mut s = Fresh::new("c1");
    s.repo.coworker_push(3).unwrap();
    s.repo.git(&["fetch", "-q"]).unwrap();
    assert_pile!(s.engine, s.root, "", "C1 fetch only: unchanged");
    assert!(s.engine.inspect_head(&s.root).unwrap().is_none());
    s.repo.git(&["pull", "-q", "--ff-only"]).unwrap();
    let pile = assert_pile!(
        s.engine,
        s.root,
        "u1 upstream|u2 upstream|u3 upstream",
        "C2 ff pull: grouped"
    );
    let groups = pile.groups();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].kind, Annotation::Upstream);
    assert_eq!(
        groups[0].paths,
        vec![b"u1".to_vec(), b"u2".to_vec(), b"u3".to_vec()]
    );
    accept_groups(&mut s);
    assert_pile!(s.engine, s.root, "", "C2 accept group -> empty");
    s.restart();
    assert_pile!(s.engine, s.root, "");
}

#[test]
fn scenario_c3_merge_with_uncommitted_edits() {
    let mut s = Fresh::new("c3");
    s.repo.checkout_b("feat-x").unwrap();
    s.repo.write("fx", "fx\n");
    s.repo.commit("fx").unwrap();
    assert!(s.accept_all().ok());
    let _ = s.engine.inspect_head(&s.root).unwrap();
    s.repo.write("parse.rs", "p\n");
    s.repo.write("lexer.rs", "l\n");
    s.repo.coworker_push(4).unwrap();
    s.repo.git(&["fetch", "-q"]).unwrap();
    s.repo
        .git(&["merge", "-q", "--no-edit", "origin/main"])
        .unwrap();
    const EXPECT: &str = "lexer.rs|parse.rs|u1 upstream|u2 upstream|u3 upstream|u4 upstream";
    assert_pile!(
        s.engine,
        s.root,
        EXPECT,
        "C3 merge: uncommitted individual, upstream grouped, merge commit adds nothing"
    );
    let notice = s.head_notice().expect("merge notice");
    assert!(notice.starts_with("merged"), "{notice}");
    s.restart();
    assert_pile!(s.engine, s.root, EXPECT, "C3 restart");
}

#[test]
fn scenario_c4_conflict_during_and_after_resolution() {
    let mut s = Fresh::new("c4");
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
        s.repo
            .git(&["merge", "-q", "--no-edit", "origin/main"])
            .is_err()
    );
    let pile = assert_pile!(
        s.engine,
        s.root,
        "f2 mixed|u1 upstream",
        "C4 during conflict: f2 with markers (mixed), u1 grouped"
    );
    let row = pile.row(b"f2").unwrap();
    assert!(row.conflicted);
    let cur = s
        .store()
        .cat_blob(&row.current.as_ref().unwrap().oid)
        .unwrap();
    assert!(
        cur.starts_with(b"<<<<<<<"),
        "{}",
        String::from_utf8_lossy(&cur)
    );
    let rg = s.engine.root(&s.root).unwrap().repo.as_ref().unwrap();
    assert_eq!(
        headstate::inspect(rg).unwrap().in_progress,
        Some(InProgress::Merge)
    );
    assert!(
        s.engine
            .inspect_head(&s.root)
            .unwrap()
            .and_then(|c| c.notice)
            .is_none()
    );
    s.repo.write("f2", "resolved\n");
    s.repo.git(&["add", "f2"]).unwrap();
    s.repo.git(&["commit", "-qm", "merge"]).unwrap();
    let pile = assert_pile!(
        s.engine,
        s.root,
        "f2 mixed|u1 upstream",
        "C4 after resolution: resolution individual w/ badge, rest grouped"
    );
    assert!(!pile.row(b"f2").unwrap().conflicted);
    assert!(s.head_notice().is_some(), "a notice once the merge clears");
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "");
}

#[test]
fn scenario_c5_both_sides_touched_same_file() {
    let mut s = Fresh::new("c5");
    s.repo.checkout_b("feat-x").unwrap();
    s.repo
        .write("f1", "A1\na2\na3\na4\na5\na6\na7\na8\na9\na10\n");
    s.repo.commit("local").unwrap();
    assert!(s.accept_all().ok());
    let _ = s.engine.inspect_head(&s.root).unwrap();
    s.repo
        .coworker_commit(&[("f1", "a1\na2\na3\na4\na5\na6\na7\na8\na9\nA10\n")], "cw")
        .unwrap();
    s.repo.git(&["fetch", "-q"]).unwrap();
    s.repo
        .git(&["merge", "-q", "--no-edit", "origin/main"])
        .unwrap();
    let pile = assert_pile!(
        s.engine,
        s.root,
        "f1 mixed",
        "C5 both sides: individual row, mixed badge"
    );
    let row = pile.row(b"f1").unwrap();
    assert_eq!(row.annotation, Some(Annotation::Mixed));
    assert_eq!(
        (row.added, row.deleted),
        (1, 1),
        "only the coworker's line differs from seen"
    );
}

#[test]
fn scenario_c6_upstream_plus_uncommitted_delta_is_mixed() {
    let mut s = Fresh::new("c6");
    s.repo.coworker_push(2).unwrap();
    s.repo.git(&["pull", "-q", "--ff-only"]).unwrap();
    let u2 = std::fs::read(s.repo.path().join("u2")).unwrap();
    s.repo.write("u2", [u2.as_slice(), b"more\n"].concat());
    let pile = assert_pile!(
        s.engine,
        s.root,
        "u1 upstream|u2 mixed",
        "C6 upstream + uncommitted delta = mixed"
    );
    assert_eq!(pile.groups().len(), 1);
    assert_eq!(pile.groups()[0].paths, vec![b"u1".to_vec()]);
}

#[test]
fn scenario_c7_no_range_falls_back_to_unannotated() {
    let mut s = Fresh::new("c7");
    s.repo.write("f1", "orphan\n");
    s.repo
        .git(&["checkout", "-q", "--orphan", "orphan"])
        .unwrap();
    s.repo.git(&["add", "-A"]).unwrap();
    s.repo.commit("orphan").unwrap();
    let pile = assert_pile!(
        s.engine,
        s.root,
        "f1",
        "C7 unrelated history: plain over-show"
    );
    assert!(
        pile.row(b"f1").unwrap().annotation.is_none(),
        "no range → no annotation"
    );
    assert!(pile.groups().is_empty());
}

#[test]
fn scenario_c8_checkout_of_a_coworker_branch() {
    let mut s = Fresh::new("c8");
    s.repo
        .coworker_commit_to("feat-z", &[("z1", "z\n")], "z")
        .unwrap();
    s.repo.git(&["fetch", "-q"]).unwrap();
    s.repo.git(&["checkout", "-q", "feat-z"]).unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "z1 upstream",
        "C8 coworker branch: grouped as upstream"
    );
    assert_eq!(
        s.head_notice().as_deref(),
        Some("switched main → feat-z: 1 files differ from seen state")
    );
    accept_groups(&mut s);
    assert_pile!(s.engine, s.root, "");
}
