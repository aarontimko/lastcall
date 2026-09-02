//! Scenario suite A — core seen-state loop (docs/spec/01-scenarios.md §A). Expected pile
//! strings are copied from `scripts/harness/scenarios.sh` for the (H) scenarios.

mod common;

use common::Fresh;
use lastcall_engine::git::Oid;
use lastcall_engine::ops::{NoFault, Refused, Rendered};
use lastcall_engine::scan::Change;
use lastcall_testkit::assert_pile;

#[test]
fn scenario_a1_first_sight_of_a_git_repo() {
    let mut s = Fresh::new("a1");
    // `echo "a1 CHANGED" > f1.tmp && mv f1.tmp f1`
    s.repo.write("f1.tmp", "a1 CHANGED\n");
    std::fs::rename(s.repo.path().join("f1.tmp"), s.repo.path().join("f1")).unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "f1",
        "A1 first sight: uncommitted edit pending"
    );
    let ledger = s.ledger();
    assert_eq!(
        ledger.seen_tree.as_ref().map(Oid::as_str),
        Some(s.tree_of_head().as_str())
    );
    assert_eq!(
        ledger.seen_at.head_commit.as_ref().map(Oid::as_str),
        Some(s.head().as_str())
    );
    s.restart();
    assert_pile!(s.engine, s.root, "f1", "A1 restart");
}

#[test]
fn scenario_a2_edit_review_accept_file() {
    let mut s = Fresh::new("a2");
    s.repo.write("f1", "a1 CHANGED\n");
    assert_pile!(s.engine, s.root, "f1");
    let rendered_oid = s.row("f1").current.unwrap().oid;
    assert!(s.accept_file("f1").ok());
    assert_pile!(s.engine, s.root, "", "A2 accept file -> empty");
    let over = s.ledger().overrides.get("f1").expect("override f1");
    assert_eq!(
        over.blob,
        Some(Some(rendered_oid.clone())),
        "override f1 → blob(rendered f1)"
    );
    s.restart();
    assert_pile!(s.engine, s.root, "", "A2 restart -> empty");
    s.repo.write("f1", "a1 CHANGED\nmore\n");
    let pile = assert_pile!(
        s.engine,
        s.root,
        "f1",
        "A2 further edit pending vs override"
    );
    let row = pile.row(b"f1").unwrap();
    assert_eq!(
        row.baseline.as_ref().map(|e| &e.oid),
        Some(&rendered_oid),
        "baseline is the override"
    );
    assert_eq!((row.added, row.deleted), (1, 0), "only the new delta");
    assert_eq!(row.hunks.len(), 1);
}

#[test]
fn scenario_a3_accept_hunk() {
    let mut s = Fresh::new("a3");
    // Two separated hunks: line 1 and line 10.
    s.repo
        .write("f1", "A1\na2\na3\na4\na5\na6\na7\na8\na9\nA10\n");
    let row = s.row("f1");
    assert_eq!(row.hunks.len(), 2, "two separated hunks: {:?}", row.hunks);
    let seen_blob = row.baseline.clone().unwrap().oid;
    assert!(s.accept_hunk("f1", 0).ok());
    let pile = assert_pile!(s.engine, s.root, "f1");
    let row = pile.row(b"f1").unwrap();
    assert_eq!(row.hunks.len(), 1, "exactly hunk 2 remains");
    assert!(
        row.hunks[0].lines.iter().any(|(_, l)| l == b"A10\n"),
        "the remaining hunk is the line-10 change: {:?}",
        row.hunks[0]
    );
    // override blob = seen blob ⊕ hunk 1
    let over_oid = s.ledger().overrides["f1"]
        .blob
        .clone()
        .flatten()
        .expect("blob override");
    assert_ne!(over_oid, seen_blob);
    let bytes = s.store().cat_blob(&over_oid).unwrap();
    assert_eq!(bytes, b"A1\na2\na3\na4\na5\na6\na7\na8\na9\na10\n");
    assert_eq!(
        row.baseline.as_ref().unwrap().oid,
        over_oid,
        "baseline is now the override"
    );
}

#[test]
fn scenario_a4_accept_all_folds_to_a_new_tree() {
    let mut repo = lastcall_testkit::fixture_repo::FixtureRepo::new("a4").unwrap();
    repo.commit_files(&[("g", "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n")], "g")
        .unwrap();
    let mut s = Fresh::over(repo, Default::default(), Default::default(), false);
    // 5 files pending, 2 of them with hunk-level overrides.
    s.repo
        .write("f1", "A1\na2\na3\na4\na5\na6\na7\na8\na9\nA10\n");
    s.repo.write("g", "ONE\n2\n3\n4\n5\n6\n7\n8\n9\nTEN\n");
    s.repo.write("f2", "x\n");
    s.repo.write("f3", "y\n");
    s.repo.write("new1", "y\n");
    assert!(s.accept_hunk("f1", 0).ok());
    assert!(s.accept_hunk("g", 0).ok());
    assert_pile!(s.engine, s.root, "f1|f2|f3|g|new1");
    let snapshot = s.scan();
    let rendered_new1 = snapshot.row(b"new1").unwrap().current.clone().unwrap().oid;
    let rendered_f1 = snapshot.row(b"f1").unwrap().current.clone().unwrap().oid;
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "", "A4 accept all -> empty");
    let ledger = s.ledger();
    assert!(
        ledger.overrides.is_empty(),
        "overrides cleared: {:?}",
        ledger.overrides
    );
    let seen = ledger.seen_tree.clone().expect("seen tree");
    let tree = s.store().ls_tree(&seen).unwrap();
    assert_eq!(
        tree.get(&b"new1"[..]).map(|(_, o)| o),
        Some(&rendered_new1),
        "A4 seen tree contains new1"
    );
    assert_eq!(
        tree.get(&b"f1"[..]).map(|(_, o)| o),
        Some(&rendered_f1),
        "exact rendered blob"
    );
    s.restart();
    assert_pile!(s.engine, s.root, "", "A4 restart");
}

#[test]
fn scenario_a5_accept_all_is_cas_per_file() {
    let mut s = Fresh::new("a5");
    s.repo.write("f1", "one\n");
    s.repo.write("f2", "r1\n");
    s.repo.write("f3", "three\n");
    let snapshot = s.scan(); // the user opens confirm
    let rendered_f2 = snapshot.row(b"f2").unwrap().current.clone().unwrap().oid;
    s.repo.write("f2", "r2\n"); // the agent rewrites f2 before the user confirms
    assert!(s.accept_all_snapshot(&snapshot).ok());
    let pile = assert_pile!(
        s.engine,
        s.root,
        "f2",
        "A5 accept-all blesses rendered f2, live delta pending"
    );
    let row = pile.row(b"f2").unwrap();
    assert_eq!(
        row.baseline.as_ref().map(|e| &e.oid),
        Some(&rendered_f2),
        "baseline = what the user saw"
    );
    let seen = s.ledger().seen_tree.clone().unwrap();
    let tree = s.store().ls_tree(&seen).unwrap();
    assert_eq!(
        tree[&b"f2"[..]].1,
        rendered_f2,
        "new tree contains the rendered f2, not the live one"
    );
}

#[test]
fn scenario_a6_accept_file_is_cas() {
    let mut s = Fresh::new("a6");
    s.repo.write("f1", "first\n");
    let rendered = Rendered::of(&s.row("f1"));
    s.repo.write("f1", "second\n"); // between render and click
    let out = s
        .engine
        .ops(&s.root)
        .unwrap()
        .accept_file(&rendered, &NoFault)
        .unwrap();
    assert!(
        matches!(&out.refused[..], [Refused::Moved { path, .. }] if path == b"f1"),
        "{out:?}"
    );
    assert!(!out.written);
    assert!(s.ledger().overrides.is_empty(), "nothing recorded");
    let pile = assert_pile!(s.engine, s.root, "f1");
    let live = s.store().hash_bytes(b"second\n").unwrap();
    assert_eq!(
        pile.row(b"f1").unwrap().current.as_ref().unwrap().oid,
        live,
        "re-rendered with the newer content"
    );
}

#[test]
fn scenario_a7_accept_deletion_and_recreate() {
    let mut s = Fresh::new("a7");
    s.repo.remove("f3");
    let pile = assert_pile!(s.engine, s.root, "f3", "A7 deletion pending");
    assert_eq!(pile.row(b"f3").unwrap().change, Change::Deleted);
    assert!(s.accept_file("f3").ok());
    assert_pile!(s.engine, s.root, "", "A7 accept deletion -> empty");
    assert_eq!(
        s.ledger().overrides["f3"].blob,
        Some(None),
        "override f3: null"
    );
    s.repo.write("f3", "c\n");
    let pile = assert_pile!(
        s.engine,
        s.root,
        "f3",
        "A7 recreate after accepted deletion -> pending"
    );
    let row = pile.row(b"f3").unwrap();
    assert_eq!(row.change, Change::Added);
    assert!(row.baseline.is_none(), "baseline is absent");
}

#[test]
fn scenario_a8_flag_with_note_retained_through_accept_all() {
    let mut s = Fresh::new("a8");
    s.repo
        .write("f1", "A1\na2\na3\na4\na5\na6\na7\na8\na9\nA10\n");
    assert_pile!(s.engine, s.root, "f1");
    let before = s.row("f1");
    let out = s
        .engine
        .ops(&s.root)
        .unwrap()
        .flag(b"f1", "why is this unwrap safe?", &NoFault)
        .unwrap();
    assert!(out.ok());
    let pile = assert_pile!(s.engine, s.root, "f1");
    let row = pile.row(b"f1").unwrap();
    assert_eq!(
        row.flag.as_ref().map(|f| f.note.as_str()),
        Some("why is this unwrap safe?")
    );
    assert_eq!(row.baseline, before.baseline, "baseline unchanged");
    assert_eq!(row.hunks, before.hunks, "hunks unchanged");
    let over = &s.ledger().overrides["f1"];
    assert!(over.blob.is_none(), "flag-only override has no blob");
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "");
    let over = s
        .ledger()
        .overrides
        .get("f1")
        .expect("flag retained as flag-only");
    assert!(over.blob.is_none());
    assert_eq!(
        over.flag.as_ref().map(|f| f.note.as_str()),
        Some("why is this unwrap safe?")
    );
}
