//! Scenario suite E — storage faults (docs/spec/01-scenarios.md §E): crashes, corruption,
//! missing objects, moved roots and compaction.

mod common;

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;

use common::Fresh;
use lastcall_engine::config::Config;
use lastcall_engine::engine::EngineOptions;
use lastcall_engine::git::Oid;
use lastcall_engine::ops::{FaultPoint, Rendered};
use lastcall_testkit::assert_pile;
use lastcall_testkit::engine::{KillAt, open_engine, pile_string};
use lastcall_testkit::fixture_repo::{FixtureRepo, engine_env_for};
use lastcall_testkit::tmp::TempDir;

const TWO_HUNKS: &str = "A1\na2\na3\na4\na5\na6\na7\na8\na9\nA10\n";
/// Seen f1 with hunk 1 applied: the blob the interrupted accept-hunk writes.
const HUNK1_APPLIED: &[u8] = b"A1\na2\na3\na4\na5\na6\na7\na8\na9\na10\n";

/// E1 child role: open the engine over `LC_E1_REPO` and accept hunk 0 of f1 with a fault
/// injector that SIGKILLs the process at `LC_E1_POINT`. Never returns normally.
fn e1_child() {
    let repo = std::path::PathBuf::from(std::env::var_os("LC_E1_REPO").unwrap());
    let state = std::path::PathBuf::from(std::env::var_os("LC_E1_STATE").unwrap());
    let point = match std::env::var("LC_E1_POINT").unwrap().as_str() {
        "object" => FaultPoint::AfterObjectWrite,
        "tmp" => FaultPoint::AfterLedgerTmpWrite,
        other => panic!("bad point {other}"),
    };
    let home = repo.parent().unwrap().join("home");
    let env = engine_env_for(&repo, &home, &state);
    let mut engine = open_engine(&repo, &env, &state, Config::default());
    let root = engine.roots()[0].path.clone();
    let pile = engine.scan(&root).unwrap();
    let row = pile.row(b"f1").unwrap().clone();
    let rendered = Rendered::of(&row);
    let _ = engine
        .ops(&root)
        .unwrap()
        .accept_hunk(&rendered, &row.hunks, 0, &KillAt(point));
    panic!("the fault injector should have killed this process");
}

fn e1_case(name: &str, point: &str) -> String {
    let repo = FixtureRepo::new(name).unwrap();
    let state = TempDir::new("lc-e1-state");
    let config = Config::default();
    // Parent: first sight, the edit, the oracle (what a reopened engine must show).
    let env = repo.engine_env(state.path());
    let mut engine = open_engine(repo.path(), &env, state.path(), config.clone());
    let root = engine.roots()[0].path.clone();
    repo.write("f1", TWO_HUNKS);
    let oracle = engine.scan(&root).unwrap();
    assert_eq!(pile_string(&oracle), "f1");
    assert_eq!(oracle.row(b"f1").unwrap().hunks.len(), 2);
    let (paths, novel) = {
        let rs = engine.root(&root).unwrap();
        let out = rs
            .store
            .git()
            .run_stdin(None, &["hash-object", "--stdin"], HUNK1_APPLIED)
            .unwrap();
        let novel = Oid::parse(String::from_utf8_lossy(&out).trim()).unwrap();
        assert!(
            !rs.store.exists(&novel),
            "the hunk-1 blob is novel before the accept"
        );
        (rs.paths.clone(), novel)
    };
    let ledger_before = std::fs::read(&paths.ledger).unwrap();
    drop(engine);

    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "scenario_e1_kill9_between_object_write_and_ledger_rename",
            "--nocapture",
        ])
        .env("LC_E1_ROLE", "child")
        .env("LC_E1_POINT", point)
        .env("LC_E1_REPO", repo.path())
        .env("LC_E1_STATE", state.path())
        .status()
        .unwrap();
    assert_eq!(
        status.signal(),
        Some(9),
        "child must die by SIGKILL: {status:?}"
    );

    assert_eq!(
        std::fs::read(&paths.ledger).unwrap(),
        ledger_before,
        "{point}: ledger.json is byte-identical (no torn write)"
    );
    let tmp = paths.ledger.with_extension("json.tmp");
    assert_eq!(
        tmp.exists(),
        point == "tmp",
        "{point}: ledger.json.tmp presence"
    );

    // Reopen: orphan blob tolerated, stale tmp removed, pile equals the oracle.
    let mut engine = open_engine(repo.path(), &env, state.path(), config);
    let rs = engine.root(&root).unwrap();
    assert!(
        rs.store.exists(&novel),
        "{point}: the orphan blob is still in the store"
    );
    if point == "tmp" {
        assert!(!tmp.exists(), "stale ledger.json.tmp removed at open");
        assert!(
            rs.notices.iter().any(|n| n.contains("ledger.json.tmp")),
            "notice about the stale tmp: {:?}",
            rs.notices
        );
    }
    let pile = engine.scan(&root).unwrap();
    assert_eq!(
        pile_string(&pile),
        pile_string(&oracle),
        "{point}: pile equals the oracle"
    );
    assert_eq!(
        pile.row(b"f1").unwrap().hunks,
        oracle.row(b"f1").unwrap().hunks
    );
    assert!(engine.root(&root).unwrap().ledger.overrides.is_empty());
    point.to_owned()
}

#[test]
fn scenario_e1_kill9_between_object_write_and_ledger_rename() {
    if std::env::var_os("LC_E1_ROLE").as_deref() == Some(std::ffi::OsStr::new("child")) {
        e1_child();
    }
    let ran = [e1_case("e1-object", "object"), e1_case("e1-tmp", "tmp")];
    assert_eq!(ran, ["object", "tmp"], "both fault points were exercised");
}

#[test]
fn scenario_e2_corrupt_override_falls_back_to_tree_baseline() {
    let mut s = Fresh::new("e2");
    s.repo
        .write("f1", "a1\na2\na3\na4\na5\na6\na7\na8\na9\na10\nedit\n");
    assert!(s.accept_file("f1").ok());
    assert_pile!(s.engine, s.root, "");
    let oid = s.ledger().overrides["f1"].blob.clone().flatten().unwrap();
    let ledger_path = s.engine.root(&s.root).unwrap().paths.ledger.clone();
    let text = std::fs::read_to_string(&ledger_path).unwrap();
    assert!(text.contains(oid.as_str()));
    std::fs::write(
        &ledger_path,
        text.replace(oid.as_str(), &"deadbeef".repeat(5)),
    )
    .unwrap();
    s.restart();
    let pile = assert_pile!(
        s.engine,
        s.root,
        "f1",
        "E2 corrupt override -> falls to tree baseline (over-show)"
    );
    let row = pile.row(b"f1").unwrap();
    assert_eq!(
        (row.added, row.deleted),
        (1, 0),
        "diffed against the seen-tree blob"
    );
    let notices: Vec<&String> = s
        .engine
        .root(&s.root)
        .unwrap()
        .notices
        .iter()
        .chain(pile.notices.iter())
        .collect();
    assert!(
        notices.iter().any(|n| n.contains("f1")),
        "notice names the path: {notices:?}"
    );
}

#[test]
fn scenario_e3_alternates_read_miss_fails_open() {
    let mut s = Fresh::new("e3");
    let f2 = s.store().hash_bytes(b"b\n").unwrap();
    let (dir, rest) = f2.as_str().split_at(2);
    let loose = s.repo.path().join(".git/objects").join(dir).join(rest);
    assert!(loose.exists(), "seed blob is loose in the user's repo");
    // Simulate a user-side gc/prune that dropped an object our seen tree references.
    std::fs::remove_file(&loose).unwrap();
    s.repo.write("f2", "b\nx\n");
    let pile = s.scan();
    assert!(
        pile.row(b"f2").is_some(),
        "f2 re-flags rather than disappearing: {pile:?}"
    );
    let notices: Vec<&String> = s
        .engine
        .root(&s.root)
        .unwrap()
        .notices
        .iter()
        .chain(pile.notices.iter())
        .collect();
    assert!(!notices.is_empty(), "a notice explains the read miss");
    assert!(!pile.row(b"f1").is_some(), "unrelated files stay clean");
}

#[test]
fn scenario_e4_root_moved_is_first_sight_with_a_hint() {
    let repo = FixtureRepo::new("e4").unwrap();
    let state = TempDir::new("lc-e4-state");
    let parent = repo.parent_dir().to_path_buf();
    let env = repo.engine_env(state.path());
    let mut engine = open_engine(&parent, &env, state.path(), Config::default());
    let old_root = std::fs::canonicalize(repo.path()).unwrap();
    repo.write("f1", "a1\na2\na3\na4\na5\na6\na7\na8\na9\na10\nedit\n");
    let pile = engine.scan(&old_root).unwrap();
    let rendered = Rendered::of(pile.row(b"f1").unwrap());
    engine
        .ops(&old_root)
        .unwrap()
        .accept_file(&rendered, &lastcall_engine::ops::NoFault)
        .unwrap();
    assert_eq!(pile_string(&engine.scan(&old_root).unwrap()), "");
    let old_state_dir = engine.root(&old_root).unwrap().paths.repo_dir.clone();
    drop(engine);

    let new_path = parent.join("e4-moved");
    std::fs::rename(repo.path(), &new_path).unwrap();
    let env = engine_env_for(&new_path, &parent.join("home"), state.path());
    let mut engine = open_engine(&parent, &env, state.path(), Config::default());
    let new_root = std::fs::canonicalize(&new_path).unwrap();
    let rs = engine
        .root(&new_root)
        .expect("the moved repo is discovered under the parent");
    assert_ne!(
        rs.paths.repo_dir, old_state_dir,
        "different root id → different state dir"
    );
    assert!(
        old_state_dir.join("ledger.json").exists(),
        "old state is kept, never gc'd"
    );
    let old_str = old_root.to_string_lossy().to_string();
    assert!(
        rs.notices
            .iter()
            .any(|n| n.contains("no longer on disk") && n.contains(&old_str)),
        "E4 notice names the old root: {:?}",
        rs.notices
    );
    assert!(
        rs.ledger.overrides.is_empty(),
        "first sight: no overrides carried over"
    );
    assert_eq!(
        pile_string(&engine.scan(&new_root).unwrap()),
        "f1",
        "E4 first sight over-shows the edit"
    );
}

#[test]
fn scenario_e5_compaction_preserves_the_pile() {
    let options = EngineOptions {
        compaction_threshold: 10,
        ..EngineOptions::default()
    };
    let mut s = Fresh::with("e5", Config::default(), options, false);
    let mut compacted = 0;
    for i in 1..=12 {
        let name = format!("o{i}");
        s.repo.write(&name, "v\n");
        let out = s.accept_file(&name);
        assert!(out.ok(), "{out:?}");
        if out.compacted {
            compacted += 1;
        }
    }
    assert!(compacted >= 1, "at least one accept crossed the threshold");
    let ledger = s.ledger();
    assert!(
        ledger.blob_override_count() <= 10,
        "{} overrides",
        ledger.blob_override_count()
    );
    let seen = ledger.seen_tree.clone().unwrap();
    let tree = s.store().ls_tree(&seen).unwrap();
    assert!(
        tree.contains_key(&b"o1"[..]),
        "compacted overrides were folded into the seen tree"
    );
    let before = pile_string(&s.scan());
    assert_eq!(before, "");
    assert!(s.accept_all().ok());
    assert_eq!(
        pile_string(&s.scan()),
        before,
        "E5 compaction preserves pile"
    );
    s.restart();
    assert_pile!(s.engine, s.root, "");
    assert!(Path::new(&s.engine.root(&s.root).unwrap().paths.ledger).exists());
}
