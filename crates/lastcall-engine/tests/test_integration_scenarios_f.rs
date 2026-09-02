//! Scenario suite F — draft roots (docs/spec/01-scenarios.md §F): non-git dirs and
//! gitignored dirs inside a repo tracked by the same seen-tree machinery.

mod common;

use common::Fresh;
use lastcall_engine::config::{Config, DraftInitial};
use lastcall_engine::engine::EngineOptions;
use lastcall_engine::ops::NoFault;
use lastcall_engine::store::RootKind;
use lastcall_testkit::assert_pile;
use lastcall_testkit::engine::{open_engine, pile_string};
use lastcall_testkit::fixture_repo::{FixtureRepo, engine_env_for};
use lastcall_testkit::tmp::TempDir;

#[test]
fn scenario_f1_gitignored_draft_dir_inside_a_repo() {
    let mut repo = FixtureRepo::new("f1").unwrap();
    repo.commit_files(&[(".gitignore", "_drafts/\n")], "ignore drafts")
        .unwrap();
    repo.write("_drafts/reply.md", "draft\n");
    let config = Config {
        draft_dirs: vec!["_drafts".to_owned()],
        ..Config::default()
    };
    let mut s = Fresh::over(repo, config, EngineOptions::default(), false);
    let draft = std::fs::canonicalize(s.repo.path().join("_drafts")).unwrap();
    let rs = s.engine.root(&draft).expect("_drafts is a draft root");
    assert_eq!(rs.kind, RootKind::Draft);
    assert_pile!(s.engine, s.root, "", "F1 git root ignores the draft dir");
    assert_pile!(s.engine, draft, "", "F1 draft first sight (seen)");
    s.repo.write("_drafts/reply.md", "draft v2\n");
    assert_pile!(s.engine, draft, "reply.md", "F1 draft edit pending");
    assert_pile!(s.engine, s.root, "", "F1 git root still clean");
    let pile = s.engine.scan(&draft).unwrap();
    let out = s
        .engine
        .ops(&draft)
        .unwrap()
        .accept_all(&pile, &NoFault)
        .unwrap();
    assert!(out.ok());
    assert_pile!(s.engine, draft, "", "F1 accept all");
    s.restart();
    assert_pile!(s.engine, draft, "");
}

#[test]
fn scenario_f2_draft_initial_pending() {
    let parent = TempDir::new("lc-f2");
    parent.write("notes2/a", "a\n");
    let state = TempDir::new("lc-f2-state");
    let env = engine_env_for(parent.path(), &parent.join("home"), state.path());
    let config = Config {
        draft_dirs: vec!["notes2".to_owned()],
        draft_initial: DraftInitial::Pending,
        ..Config::default()
    };
    let mut engine = open_engine(parent.path(), &env, state.path(), config);
    let root = std::fs::canonicalize(parent.join("notes2")).unwrap();
    assert_eq!(engine.root_paths(), vec![root.clone()]);
    assert_pile!(engine, root, "a", "F2 draft_initial=pending");
    // `pending` first sight anchors to *no* tree: every file's baseline is Empty until
    // accepted, exactly like an unreadable ledger (fail open).
    assert!(engine.root(&root).unwrap().ledger.seen_tree.is_none());
    let pile = engine.scan(&root).unwrap();
    assert!(
        engine
            .ops(&root)
            .unwrap()
            .accept_all(&pile, &NoFault)
            .unwrap()
            .ok()
    );
    assert_pile!(engine, root, "");
}

#[test]
fn scenario_f3_non_git_draft_dir() {
    let parent = TempDir::new("lc-f3");
    for i in 1..=5 {
        parent.write(format!("notes/n{i}.md"), "n\n");
    }
    let state = TempDir::new("lc-f3-state");
    let env = engine_env_for(parent.path(), &parent.join("home"), state.path());
    let config = Config {
        draft_dirs: vec!["notes".to_owned()],
        ..Config::default()
    };
    let mut engine = open_engine(parent.path(), &env, state.path(), config.clone());
    let root = std::fs::canonicalize(parent.join("notes")).unwrap();
    assert_eq!(engine.root(&root).unwrap().kind, RootKind::Draft);
    assert_pile!(engine, root, "", "F3 non-git draft dir first sight (seen)");
    parent.write("notes/n2.md", "changed\n");
    parent.write("notes/n9.md", "new\n");
    assert_pile!(engine, root, "n2.md|n9.md", "F3 draft edits pending");
    let pile = engine.scan(&root).unwrap();
    assert!(
        engine
            .ops(&root)
            .unwrap()
            .accept_all(&pile, &NoFault)
            .unwrap()
            .ok()
    );
    assert_pile!(engine, root, "", "F3 accept all");
    drop(engine);
    let mut engine = open_engine(parent.path(), &env, state.path(), config);
    assert_eq!(pile_string(&engine.scan(&root).unwrap()), "", "F3 restart");
    assert!(
        engine
            .root(&root)
            .unwrap()
            .ledger
            .seen_at
            .head_commit
            .is_none(),
        "drafts have no HEAD"
    );
}
