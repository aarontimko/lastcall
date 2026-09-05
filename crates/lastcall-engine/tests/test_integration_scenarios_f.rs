//! Scenario suite F — draft roots (docs/spec/01-scenarios.md §F): non-git dirs and
//! gitignored dirs inside a repo tracked by the same seen-tree machinery.

mod common;

use common::Fresh;
use lastcall_engine::config::{Config, DraftInitial};
use lastcall_engine::engine::EngineOptions;
use lastcall_engine::ops::{NoFault, Rendered};
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

    // Phase 6 gate item 1: the same gitignored draft file goes pending, is reviewed one
    // hunk at a time, and every step survives a restart. A 20-line baseline so two edits
    // six lines apart are two hunks at CONTEXT 3 and not one merged hunk.
    let baseline: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    s.repo.write("_drafts/reply.md", &baseline);
    assert_pile!(
        s.engine,
        draft,
        "reply.md",
        "F1 the 20-line baseline is pending"
    );
    assert!(accept_file(&mut s, &draft, "reply.md").ok());
    assert_pile!(
        s.engine,
        draft,
        "",
        "F1 the 20-line baseline is the seen point"
    );

    // The second edit: two regions far enough apart to be two hunks.
    let edited: String = (1..=20)
        .map(|i| match i {
            2 => "line 2 edited by the agent\n".to_owned(),
            18 => "line 18 edited by the agent\n".to_owned(),
            _ => format!("line {i}\n"),
        })
        .collect();
    s.repo.write("_drafts/reply.md", &edited);
    let pile = assert_pile!(s.engine, draft, "reply.md", "F1 hunk-review: pending");
    let row = pile.row(b"reply.md").unwrap();
    assert_eq!(row.hunks.len(), 2, "F1: the second edit is two hunks");
    assert_eq!(
        row.collapsed, None,
        "F1: a small text draft is not collapsed"
    );

    // Accept the first hunk only; the second stays pending.
    let rendered = Rendered::of(row);
    let out = s
        .engine
        .ops(&draft)
        .unwrap()
        .accept_hunk(&rendered, &row.hunks, 0, &NoFault)
        .unwrap();
    assert!(out.ok(), "F1 accept_hunk: {out:?}");
    let after = assert_pile!(s.engine, draft, "reply.md", "F1 one hunk left");
    let left = remaining_hunk(&after);
    assert_eq!(
        left, "line 18 edited by the agent\n",
        "F1: the accepted hunk is gone, the untouched one remains"
    );

    // …and survives a restart: the ledger override is the baseline, recomputed fresh.
    s.restart();
    let after_restart = assert_pile!(s.engine, draft, "reply.md", "F1 restart keeps the hunk");
    assert_eq!(
        remaining_hunk(&after_restart),
        left,
        "F1: the same remaining hunk after a restart"
    );

    // Accept the file: the row clears, and stays clear across a second restart.
    assert!(accept_file(&mut s, &draft, "reply.md").ok());
    assert_pile!(s.engine, draft, "", "F1 accept_file clears the row");
    s.restart();
    assert_pile!(s.engine, draft, "", "F1 accepted, after a second restart");
}

/// `accept_file` against an arbitrary root of `s` (`Fresh::accept_file` targets the git root).
fn accept_file(s: &mut Fresh, root: &std::path::Path, path: &str) -> lastcall_engine::ops::Outcome {
    let pile = s.engine.scan(root).expect("scan");
    let row = pile
        .row(path.as_bytes())
        .unwrap_or_else(|| panic!("{path} is not pending: {}", pile_string(&pile)));
    let rendered = Rendered::of(row);
    s.engine
        .ops(root)
        .unwrap()
        .accept_file(&rendered, &NoFault)
        .expect("accept_file")
}

/// The single insert line of the pile's one remaining hunk.
fn remaining_hunk(pile: &lastcall_engine::scan::Pile) -> String {
    let row = pile.row(b"reply.md").expect("reply.md is pending");
    assert_eq!(row.hunks.len(), 1, "exactly one hunk left: {:?}", row.hunks);
    let inserts: Vec<String> = row.hunks[0]
        .lines
        .iter()
        .filter(|(t, _)| *t == lastcall_engine::hunks::Tag::Insert)
        .map(|(_, l)| String::from_utf8_lossy(l).into_owned())
        .collect();
    inserts.join("")
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
