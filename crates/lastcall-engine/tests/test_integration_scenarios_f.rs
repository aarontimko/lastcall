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

// ---------------------------------------------------------------------------------------
// F4 to F8 — what a watched folder covers, and what it reads (Amendment v1.13).
// ---------------------------------------------------------------------------------------

/// The setup F4 to F8 share: a parent `W` holding the repository `repo1`, a folder
/// `z_ignore` inside it with a file of its own, a subfolder with a file, and a folder
/// below that with a file. `ignored` puts `z_ignore/` in `.gitignore`.
fn f_repo(name: &str, ignored: bool) -> FixtureRepo {
    let mut repo = FixtureRepo::new(name).unwrap();
    if ignored {
        repo.commit_files(
            &[(".gitignore", "z_ignore/\n")],
            "ignore the scratch folder",
        )
        .unwrap();
    }
    repo.write("z_ignore/a.md", "a\n");
    repo.write("z_ignore/research/b.md", "b\n");
    repo.write("z_ignore/research/deep/c.md", "c\n");
    repo
}

fn draft_config(entries: &[&str]) -> Config {
    Config {
        draft_dirs: entries.iter().map(|e| (*e).to_owned()).collect(),
        ..Config::default()
    }
}

/// The paths the root's record holds, in path order.
/// The record in force, not just its stored tree: a single-file accept lets a path go by
/// writing an override, and the tree itself is only rewritten at the next fold.
fn recorded(s: &Fresh, root: &std::path::Path) -> Vec<String> {
    let rs = s.engine.root(root).expect("an open root");
    let mut paths: std::collections::BTreeSet<String> = match rs.ledger.seen_tree.as_ref() {
        None => std::collections::BTreeSet::new(),
        Some(seen) => rs
            .store
            .ls_tree(seen)
            .unwrap()
            .keys()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .collect(),
    };
    for (key, o) in &rs.ledger.overrides {
        match &o.blob {
            Some(Some(_)) => {
                paths.insert(key.clone());
            }
            Some(None) => {
                paths.remove(key);
            }
            None => {}
        }
    }
    paths.into_iter().collect()
}

/// The root's pile notices that name unread files or a trim.
fn notices_about(pile: &lastcall_engine::scan::Pile, needle: &str) -> Vec<String> {
    pile.notices
        .iter()
        .filter(|n| n.contains(needle))
        .cloned()
        .collect()
}

#[test]
fn scenario_f4_plain_entry_reads_one_folder() {
    let repo = f_repo("repo1", true);
    let mut s = Fresh::over(
        repo,
        draft_config(&["z_ignore"]),
        EngineOptions::default(),
        true,
    );
    let draft = std::fs::canonicalize(s.repo.path().join("z_ignore")).unwrap();
    let rs = s.engine.root(&draft).expect("z_ignore is a watched folder");
    assert_eq!(rs.kind, RootKind::Draft);
    assert_eq!(rs.name, "repo1/z_ignore", "F4: named below the repository");
    assert_eq!(
        rs.scope.as_ref().map(|sc| sc.recursive),
        Some(false),
        "F4: a plain entry is the folder on its own"
    );
    assert_eq!(
        recorded(&s, &draft),
        vec!["a.md".to_owned()],
        "F4: first sight records the folder's own files"
    );
    assert_pile!(s.engine, draft, "", "F4 first sight");
    assert_pile!(s.engine, s.root, "", "F4 the repository ignores the folder");

    // An edit below the folder is not this root's business; an edit inside it is.
    s.repo.write("z_ignore/research/b.md", "b2\n");
    s.repo.write("z_ignore/a.md", "a2\n");
    assert_pile!(s.engine, draft, "a.md", "F4 one folder only");
    s.repo.write("z_ignore/research/deep/d.md", "d\n");
    assert_pile!(s.engine, draft, "a.md", "F4 an add below is not a row");
    s.repo.remove("z_ignore/research/b.md");
    assert_pile!(s.engine, draft, "a.md", "F4 a delete below is not a row");
    assert!(accept_file(&mut s, &draft, "a.md").ok());
    assert_pile!(s.engine, draft, "", "F4 accept");
    s.restart();
    assert_pile!(s.engine, draft, "", "F4 restart");

    // The report names nothing below the folder, calls the folder what the nav calls it,
    // and its JSON keeps the shape it has always had (R7: no new key).
    let report = lastcall_engine::status::StatusReport::build(&mut s.engine, None).expect("status");
    let json = report.to_json();
    assert!(
        !json.contains("research/"),
        "F4: no path below the folder in the report: {json}"
    );
    assert!(
        !json.contains("\"name\""),
        "F4: the JSON report gained no key: {json}"
    );
    assert!(
        report.render_human().contains("repo1/z_ignore (draft)"),
        "F4: the text report uses the engine's name: {}",
        report.render_human()
    );
}

/// F4's variant: with the folder **not** ignored, its subfolders are the repository's
/// untracked files and its own files are the watched folder's. Never both, never neither.
#[test]
fn scenario_f4_plain_entry_with_the_folder_not_ignored() {
    let repo = f_repo("repo1", false);
    let mut s = Fresh::over(
        repo,
        draft_config(&["z_ignore"]),
        EngineOptions::default(),
        true,
    );
    let draft = std::fs::canonicalize(s.repo.path().join("z_ignore")).unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "z_ignore/research/b.md|z_ignore/research/deep/c.md",
        "F4 variant: what the folder does not cover is the repository's"
    );
    assert_pile!(
        s.engine,
        draft,
        "",
        "F4 variant: the folder's own file is its"
    );

    s.repo.write("z_ignore/research/b.md", "b2\n");
    assert_pile!(
        s.engine,
        s.root,
        "z_ignore/research/b.md|z_ignore/research/deep/c.md",
        "F4 variant: the edit shows under the repository"
    );
    assert_pile!(s.engine, draft, "", "F4 variant: and not under the folder");
}

#[test]
fn scenario_f5_double_star_reads_the_tree() {
    let repo = f_repo("repo1", true);
    let mut s = Fresh::over(
        repo,
        draft_config(&["z_ignore/**"]),
        EngineOptions::default(),
        true,
    );
    let draft = std::fs::canonicalize(s.repo.path().join("z_ignore")).unwrap();
    assert_eq!(
        s.engine
            .root(&draft)
            .unwrap()
            .scope
            .as_ref()
            .map(|sc| sc.recursive),
        Some(true)
    );
    assert_eq!(
        recorded(&s, &draft),
        vec![
            "a.md".to_owned(),
            "research/b.md".to_owned(),
            "research/deep/c.md".to_owned()
        ],
        "F5: first sight records the tree"
    );
    assert_pile!(s.engine, draft, "", "F5 first sight");

    s.repo.write("z_ignore/research/b.md", "b2\n");
    s.repo.write("z_ignore/a.md", "a2\n");
    assert_pile!(s.engine, draft, "a.md|research/b.md", "F5 both edits");
    s.repo.write("z_ignore/research/deep/d.md", "d\n");
    let pile = assert_pile!(
        s.engine,
        draft,
        "a.md|research/b.md|research/deep/d.md",
        "F5 the add below is a row"
    );
    assert_eq!(
        pile.row(b"research/deep/d.md").unwrap().change,
        lastcall_engine::scan::Change::Added
    );
    s.repo.remove("z_ignore/research/b.md");
    let pile = assert_pile!(
        s.engine,
        draft,
        "a.md|research/b.md|research/deep/d.md",
        "F5 the delete below is a row"
    );
    assert_eq!(
        pile.row(b"research/b.md").unwrap().change,
        lastcall_engine::scan::Change::Deleted
    );
}

#[test]
fn scenario_f6_large_files_are_never_read() {
    let max = Config::default().collapse_size_bytes as usize;
    let repo = f_repo("repo1", true);
    repo.write("z_ignore/big.bin", vec![b'x'; max]);
    repo.write("z_ignore/small.bin", vec![b'x'; max - 1]);
    let mut s = Fresh::over(
        repo,
        draft_config(&["z_ignore"]),
        EngineOptions::default(),
        true,
    );
    let draft = std::fs::canonicalize(s.repo.path().join("z_ignore")).unwrap();
    assert_eq!(
        recorded(&s, &draft),
        vec!["a.md".to_owned(), "small.bin".to_owned()],
        "F6: the large file is not recorded"
    );
    // The store never took a copy of it.
    let big_oid = s.repo.git(&["hash-object", "z_ignore/big.bin"]).unwrap();
    let big_oid = lastcall_engine::git::Oid::parse(big_oid.trim()).unwrap();
    assert!(
        !s.engine.root(&draft).unwrap().store.exists(&big_oid),
        "F6: the store holds no blob of the large file"
    );
    let pile = assert_pile!(s.engine, draft, "", "F6 first sight");
    assert_eq!(
        notices_about(&pile, "not read"),
        vec!["1 file over 512 KiB not read"]
    );

    // A recorded file that grows to the limit keeps its row and says it was not read.
    s.repo.write("z_ignore/small.bin", vec![b'x'; max]);
    let pile = assert_pile!(s.engine, draft, "small.bin", "F6 the grown file");
    let row = pile.row(b"small.bin").unwrap();
    assert!(matches!(
        row.collapsed,
        Some(lastcall_engine::scan::Collapsed::Unread { .. })
    ));
    assert!(row.current.is_none(), "F6: no current-side hash");
    assert!(row.hunks.is_empty());
    let grown_oid = s.repo.git(&["hash-object", "z_ignore/small.bin"]).unwrap();
    let grown_oid = lastcall_engine::git::Oid::parse(grown_oid.trim()).unwrap();
    assert!(
        !s.engine.root(&draft).unwrap().store.exists(&grown_oid),
        "F6: the grown content was not copied in either"
    );
    assert_eq!(
        notices_about(&pile, "not read"),
        vec!["1 file over 512 KiB not read"],
        "F6: a row is not counted"
    );

    // Accepting it removes it from the record; it is then simply one of the counted files.
    assert!(accept_file(&mut s, &draft, "small.bin").ok());
    let pile = assert_pile!(s.engine, draft, "", "F6 accepted");
    assert_eq!(recorded(&s, &draft), vec!["a.md".to_owned()]);
    assert_eq!(
        notices_about(&pile, "not read"),
        vec!["2 files over 512 KiB not read"]
    );

    // Undo puts the entry and the row back.
    assert!(s.engine.ops(&draft).unwrap().undo(&NoFault).unwrap().ok());
    let pile = assert_pile!(s.engine, draft, "small.bin", "F6 undo");
    assert!(matches!(
        pile.row(b"small.bin").unwrap().collapsed,
        Some(lastcall_engine::scan::Collapsed::Unread { .. })
    ));
    assert_eq!(
        notices_about(&pile, "not read"),
        vec!["1 file over 512 KiB not read"]
    );
    assert!(accept_file(&mut s, &draft, "small.bin").ok());
    assert_pile!(s.engine, draft, "", "F6 accepted again");

    // A file that shrinks below the limit is read again, as a new file.
    s.repo.write("z_ignore/big.bin", vec![b'x'; 10]);
    let pile = assert_pile!(s.engine, draft, "big.bin", "F6 the truncated file");
    assert_eq!(
        pile.row(b"big.bin").unwrap().change,
        lastcall_engine::scan::Change::Added
    );
    assert_eq!(
        notices_about(&pile, "not read"),
        vec!["1 file over 512 KiB not read"]
    );

    // One line per scan whatever the count, never two.
    s.repo.write("z_ignore/big2.bin", vec![b'x'; max + 5]);
    let pile = assert_pile!(s.engine, draft, "big.bin", "F6 a second large file");
    assert_eq!(
        notices_about(&pile, "not read"),
        vec!["2 files over 512 KiB not read"]
    );
}

/// F6's variant: the same rule inside the tree of a folder watched with `/**`.
#[test]
fn scenario_f6_large_files_in_the_tree_are_never_read() {
    let max = Config::default().collapse_size_bytes as usize;
    let repo = f_repo("repo1", true);
    repo.write("z_ignore/research/huge.bin", vec![b'x'; max]);
    let mut s = Fresh::over(
        repo,
        draft_config(&["z_ignore/**"]),
        EngineOptions::default(),
        true,
    );
    let draft = std::fs::canonicalize(s.repo.path().join("z_ignore")).unwrap();
    assert_eq!(
        recorded(&s, &draft),
        vec![
            "a.md".to_owned(),
            "research/b.md".to_owned(),
            "research/deep/c.md".to_owned()
        ],
        "F6 variant: the large file in the tree is not recorded"
    );
    let pile = assert_pile!(s.engine, draft, "", "F6 variant: first sight");
    assert_eq!(
        notices_about(&pile, "not read"),
        vec!["1 file over 512 KiB not read"]
    );
}

/// F6's upgrade form: a record written when the limit was higher holds a large file's
/// content. It is not read, not counted and not a row while it is unchanged; it becomes an
/// unread row when it changes, and is counted once the reader accepts that row.
#[test]
fn scenario_f6_a_record_that_already_holds_a_large_file() {
    let max = Config::default().collapse_size_bytes as usize;
    let repo = f_repo("repo1", true);
    repo.write("z_ignore/big.bin", vec![b'x'; max]);
    let wide = Config {
        collapse_size_bytes: (max * 4) as u64,
        ..draft_config(&["z_ignore"])
    };
    let mut s = Fresh::over(repo, wide, EngineOptions::default(), true);
    let draft = std::fs::canonicalize(s.repo.path().join("z_ignore")).unwrap();
    assert_eq!(
        recorded(&s, &draft),
        vec!["a.md".to_owned(), "big.bin".to_owned()],
        "the earlier release read it"
    );
    assert_pile!(s.engine, draft, "", "the record is the disk");

    // The limit comes down to the shipping default.
    s.restart_with(draft_config(&["z_ignore"]));
    let pile = assert_pile!(s.engine, draft, "", "unchanged: not a row");
    assert!(
        notices_about(&pile, "not read").is_empty(),
        "unchanged: not counted either"
    );
    assert_eq!(
        recorded(&s, &draft),
        vec!["a.md".to_owned(), "big.bin".to_owned()],
        "and never dropped from the record"
    );

    s.repo.write("z_ignore/big.bin", vec![b'y'; max]);
    let pile = assert_pile!(s.engine, draft, "big.bin", "edited: an unread row");
    assert!(matches!(
        pile.row(b"big.bin").unwrap().collapsed,
        Some(lastcall_engine::scan::Collapsed::Unread { .. })
    ));
    assert!(notices_about(&pile, "not read").is_empty());

    // Verifier F3: restore is not offered on that row. It was never read, so there is
    // nothing to put back, and the engine must not read `oid: None` as a deletion and
    // remove the file the size rule deliberately left alone.
    let rendered = Rendered::of(pile.row(b"big.bin").unwrap());
    let out = s
        .engine
        .restore(
            &draft,
            lastcall_engine::engine::RestoreRequest::File(rendered),
        )
        .expect("restore call");
    assert_eq!(
        out.outcome
            .refused
            .iter()
            .map(|r| r.message("restored"))
            .collect::<Vec<_>>(),
        vec!["big.bin: not read; restore is not offered".to_owned()]
    );
    assert!(!out.outcome.written);
    assert_eq!(
        std::fs::read(s.repo.path().join("z_ignore/big.bin")).unwrap(),
        vec![b'y'; max],
        "F3: the bytes the folder never read are untouched"
    );

    assert!(accept_file(&mut s, &draft, "big.bin").ok());
    let pile = assert_pile!(s.engine, draft, "", "accepted");
    assert_eq!(recorded(&s, &draft), vec!["a.md".to_owned()]);
    assert_eq!(
        notices_about(&pile, "not read"),
        vec!["1 file over 512 KiB not read"]
    );
}

/// Verifier F1: a file the folder has never recorded but the reader has flagged keeps its
/// row when it grows past the limit. Accepting it lets the path go (counted from then on,
/// the note kept on the override); undo brings the row back.
#[test]
fn scenario_f6_a_flagged_new_file_that_grows() {
    let max = Config::default().collapse_size_bytes as usize;
    let repo = f_repo("repo1", true);
    let mut s = Fresh::over(
        repo,
        draft_config(&["z_ignore"]),
        EngineOptions::default(),
        true,
    );
    let draft = std::fs::canonicalize(s.repo.path().join("z_ignore")).unwrap();
    s.repo.write("z_ignore/newf.txt", "new\n");
    let pile = assert_pile!(s.engine, draft, "newf.txt", "F1 a new file");
    assert_eq!(
        pile.row(b"newf.txt").unwrap().change,
        lastcall_engine::scan::Change::Added
    );
    assert!(
        s.engine
            .ops(&draft)
            .unwrap()
            .flag(b"newf.txt", "agent says look", None, None, &NoFault)
            .unwrap()
            .ok()
    );

    // It grows past the limit. The content is never read, and the row stays with its note.
    s.repo.write("z_ignore/newf.txt", vec![b'x'; max]);
    let pile = assert_pile!(s.engine, draft, "newf.txt", "F1 the flagged file grew");
    let row = pile.row(b"newf.txt").unwrap();
    assert_eq!(
        row.change,
        lastcall_engine::scan::Change::Added,
        "F1: the record holds no content for it"
    );
    assert!(matches!(
        row.collapsed,
        Some(lastcall_engine::scan::Collapsed::Unread { .. })
    ));
    assert!(row.baseline.is_none() && row.current.is_none());
    assert_eq!(
        row.flags.len(),
        1,
        "F1: the note is on the row, not stranded in the ledger"
    );
    assert!(
        notices_about(&pile, "not read").is_empty(),
        "F1: a row is not counted"
    );
    let grown_oid = s.repo.git(&["hash-object", "z_ignore/newf.txt"]).unwrap();
    let grown_oid = lastcall_engine::git::Oid::parse(grown_oid.trim()).unwrap();
    assert!(
        !s.engine.root(&draft).unwrap().store.exists(&grown_oid),
        "F1: still never read"
    );

    // Accepting it lets the path go: counted from then on, and the note is kept.
    assert!(accept_file(&mut s, &draft, "newf.txt").ok());
    let pile = assert_pile!(s.engine, draft, "", "F1 accepted");
    assert_eq!(
        notices_about(&pile, "not read"),
        vec!["1 file over 512 KiB not read"]
    );
    assert_eq!(
        s.engine.root(&draft).unwrap().ledger.overrides["newf.txt"]
            .flags
            .len(),
        1,
        "F1: accepting a row never drops the reader's note"
    );

    // And undo puts the row back, note and all.
    assert!(s.engine.ops(&draft).unwrap().undo(&NoFault).unwrap().ok());
    let pile = assert_pile!(s.engine, draft, "newf.txt", "F1 undo");
    assert_eq!(pile.row(b"newf.txt").unwrap().flags.len(), 1);
    assert!(notices_about(&pile, "not read").is_empty());

    // Verifier G1: the accept has to survive every fold. A fold that cleared the release
    // would leave the note as a flag-only override, which the record reads as "this path is
    // mine", and the row the user accepted would be back on the next scan.
    let released = |s: &Fresh, path: &str, when: &str| {
        let rs = s.engine.root(&draft).unwrap();
        let o = &rs.ledger.overrides[path];
        assert_eq!(o.blob, Some(None), "G1 {path} {when}: still a release");
        assert_eq!(o.flags.len(), 1, "G1 {path} {when}: the note stays");
    };

    // (a) an accept-all over some other pending file.
    assert!(accept_file(&mut s, &draft, "newf.txt").ok());
    released(&s, "newf.txt", "after the accept");
    s.repo.write("z_ignore/a.md", "a2\n");
    let pile = assert_pile!(s.engine, draft, "a.md", "G1 another file is pending");
    assert!(
        s.engine
            .ops(&draft)
            .unwrap()
            .accept_all(&pile, &NoFault)
            .unwrap()
            .ok()
    );
    let pile = assert_pile!(s.engine, draft, "", "G1 after an accept-all");
    assert_eq!(
        notices_about(&pile, "not read"),
        vec!["1 file over 512 KiB not read"],
        "G1: counted, not shown"
    );
    released(&s, "newf.txt", "after an accept-all over another file");

    // (b) the bounded compaction, which runs on its own once enough overrides pile up.
    s.engine.ops(&draft).unwrap().compact(&NoFault).unwrap();
    let pile = assert_pile!(s.engine, draft, "", "G1 after a compaction");
    assert_eq!(
        notices_about(&pile, "not read"),
        vec!["1 file over 512 KiB not read"]
    );
    released(&s, "newf.txt", "after a compaction");

    // (c) an accept-all over a pile that holds an unread row itself: a second flagged file
    // the folder never recorded, already over the limit. The accept-all is the accept, so
    // it lets the path go exactly as the single accept did, note and all.
    s.repo.write("z_ignore/other.bin", vec![b'z'; max]);
    assert!(
        s.engine
            .ops(&draft)
            .unwrap()
            .flag(b"other.bin", "this one too", None, None, &NoFault)
            .unwrap()
            .ok()
    );
    let pile = assert_pile!(s.engine, draft, "other.bin", "G1 a second unread row");
    assert!(matches!(
        pile.row(b"other.bin").unwrap().collapsed,
        Some(lastcall_engine::scan::Collapsed::Unread { .. })
    ));
    assert!(
        s.engine
            .ops(&draft)
            .unwrap()
            .accept_all(&pile, &NoFault)
            .unwrap()
            .ok()
    );
    let pile = assert_pile!(s.engine, draft, "", "G1 after the accept-all over the row");
    assert_eq!(
        notices_about(&pile, "not read"),
        vec!["2 files over 512 KiB not read"]
    );
    released(&s, "newf.txt", "after the third fold");
    released(&s, "other.bin", "after the accept-all that released it");
    assert_eq!(recorded(&s, &draft), vec!["a.md".to_owned()]);

    // And a restart reads the same record back off disk.
    s.restart();
    let pile = assert_pile!(s.engine, draft, "", "G1 after a restart");
    assert_eq!(
        notices_about(&pile, "not read"),
        vec!["2 files over 512 KiB not read"]
    );
    released(&s, "newf.txt", "after a restart");
}

#[test]
fn scenario_f7_scope_change_trims_and_widens() {
    let repo = f_repo("repo1", true);
    let mut s = Fresh::over(
        repo,
        draft_config(&["z_ignore/**"]),
        EngineOptions::default(),
        true,
    );
    let draft = std::fs::canonicalize(s.repo.path().join("z_ignore")).unwrap();
    assert_eq!(recorded(&s, &draft).len(), 3, "F7: the tree was recorded");
    assert!(
        s.engine
            .ops(&draft)
            .unwrap()
            .flag(b"research/b.md", "look at this", None, None, &NoFault)
            .unwrap()
            .ok()
    );
    let seen_at = s.engine.root(&draft).unwrap().ledger.seen_at.clone();

    // The entry loses its `/**`; a configuration change takes effect at the next start.
    s.restart_with(draft_config(&["z_ignore"]));
    let pile = assert_pile!(s.engine, draft, "", "F7 nothing pending after the trim");
    assert_eq!(
        notices_about(&pile, "outside the root's scope"),
        vec!["2 paths outside the root's scope dropped from its record"]
    );
    assert_eq!(recorded(&s, &draft), vec!["a.md".to_owned()]);
    let rs = s.engine.root(&draft).unwrap();
    assert_eq!(rs.ledger.seen_at, seen_at, "F7: `seen_at` untouched");
    assert!(rs.ledger.undo.is_empty(), "F7: the trim is not an accept");
    assert_eq!(
        rs.ledger.overrides["research/b.md"].flags.len(),
        1,
        "F7: the reader's note survives the trim"
    );

    // Said once, not at every scan.
    let pile = s.engine.scan(&draft).unwrap();
    assert!(notices_about(&pile, "outside the root's scope").is_empty());

    // Widening it again brings the files back as new, with the note still on one of them.
    s.restart_with(draft_config(&["z_ignore/**"]));
    let pile = assert_pile!(
        s.engine,
        draft,
        "research/b.md|research/deep/c.md",
        "F7 widened: pending as new, never hidden"
    );
    assert_eq!(
        pile.row(b"research/b.md").unwrap().flags.len(),
        1,
        "F7: and the note came back with the path"
    );
    assert_eq!(
        pile.row(b"research/b.md").unwrap().change,
        lastcall_engine::scan::Change::Added
    );

    // Verifier F2: a single-file accept of a deep new file puts it in the record as an
    // override alone, with the seen tree none the wiser. Narrowing the entry again must
    // drop it on the very next scan, not leave it for a later fold to spring on the
    // reader.
    s.repo.write("z_ignore/research/new.md", "n\n");
    assert!(accept_file(&mut s, &draft, "research/new.md").ok());
    {
        let rs = s.engine.root(&draft).unwrap();
        assert!(
            !rs.tree.contains_key(b"research/new.md".as_slice()),
            "F2: the seen tree never held the deep path"
        );
        assert!(
            matches!(
                rs.ledger.overrides.get("research/new.md").map(|o| &o.blob),
                Some(Some(Some(_)))
            ),
            "F2: the accept is an override, not a tree entry"
        );
    }
    s.restart_with(draft_config(&["z_ignore"]));
    let pile = s.engine.scan(&draft).unwrap();
    assert_eq!(
        notices_about(&pile, "outside the root's scope"),
        vec!["1 path outside the root's scope dropped from its record"],
        "F2: the trim fires for an override the tree never held"
    );
    assert!(
        !s.engine
            .root(&draft)
            .unwrap()
            .ledger
            .overrides
            .contains_key("research/new.md"),
        "F2: and the out-of-scope override is gone"
    );
    let pile = s.engine.scan(&draft).unwrap();
    assert!(
        notices_about(&pile, "outside the root's scope").is_empty(),
        "F2: said once, with no surprise left for a later fold"
    );
}

#[test]
fn scenario_f8_nested_draft_roots() {
    let repo = f_repo("repo1", true);
    let mut s = Fresh::over(
        repo,
        draft_config(&["z_ignore/**", "z_ignore/research"]),
        EngineOptions::default(),
        true,
    );
    let outer = std::fs::canonicalize(s.repo.path().join("z_ignore")).unwrap();
    let inner = std::fs::canonicalize(s.repo.path().join("z_ignore/research")).unwrap();
    assert_eq!(s.engine.root(&outer).unwrap().name, "repo1/z_ignore");
    assert_eq!(
        s.engine.root(&inner).unwrap().name,
        "repo1/z_ignore/research"
    );
    assert_eq!(
        recorded(&s, &outer),
        vec!["a.md".to_owned(), "research/deep/c.md".to_owned()],
        "F8: the inner folder reads its own files only, so the one below it is the outer's"
    );
    assert_eq!(recorded(&s, &inner), vec!["b.md".to_owned()]);

    s.repo.write("z_ignore/research/b.md", "b2\n");
    assert_pile!(s.engine, inner, "b.md", "F8 the inner folder's file");
    assert_pile!(s.engine, outer, "", "F8 and not the outer's");
    s.repo.write("z_ignore/research/deep/c.md", "c2\n");
    assert_pile!(s.engine, outer, "research/deep/c.md", "F8 the outer's file");
    assert_pile!(s.engine, inner, "b.md", "F8 and not the inner's");

    // With the inner folder reading its own tree, the file below it is the inner's.
    let repo = f_repo("repo1", true);
    let mut s = Fresh::over(
        repo,
        draft_config(&["z_ignore/**", "z_ignore/research/**"]),
        EngineOptions::default(),
        true,
    );
    let outer = std::fs::canonicalize(s.repo.path().join("z_ignore")).unwrap();
    let inner = std::fs::canonicalize(s.repo.path().join("z_ignore/research")).unwrap();
    assert_eq!(recorded(&s, &outer), vec!["a.md".to_owned()]);
    assert_eq!(
        recorded(&s, &inner),
        vec!["b.md".to_owned(), "deep/c.md".to_owned()]
    );
    s.repo.write("z_ignore/research/deep/c.md", "c2\n");
    assert_pile!(s.engine, inner, "deep/c.md", "F8 the inner folder's tree");
    assert_pile!(s.engine, outer, "", "F8 and not the outer's");
}
