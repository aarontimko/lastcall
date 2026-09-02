//! Scenario suite D — file-system and repo-shape edge cases (docs/spec/01-scenarios.md §D).

mod common;

use common::Fresh;
use lastcall_engine::config::Config;
use lastcall_engine::engine::EngineOptions;
use lastcall_engine::git::Mode;
use lastcall_engine::roots::Badge;
use lastcall_engine::scan::{Change, Collapsed, Rename, probe_case_insensitive};
use lastcall_testkit::assert_pile;
use lastcall_testkit::fixture_repo::FixtureRepo;

#[test]
fn scenario_d1_mode_only_change() {
    let mut s = Fresh::new("d1");
    s.repo.chmod_x("f2", true);
    let pile = assert_pile!(s.engine, s.root, "f2", "D1 mode-only change pending");
    let row = pile.row(b"f2").unwrap();
    assert_eq!(row.change, Change::Mode);
    assert_eq!(row.hunks.len(), 1, "one mode hunk");
    assert!(row.hunks[0].is_mode_change());
    assert_eq!(
        row.baseline.as_ref().unwrap().oid,
        row.current.as_ref().unwrap().oid,
        "same blob"
    );
    assert!(s.accept_file("f2").ok());
    assert_pile!(s.engine, s.root, "", "D1 accept records mode");
    let over = &s.ledger().overrides["f2"];
    assert_eq!(over.mode, Some(Mode::Executable));
    assert_eq!(
        over.blob.clone().flatten(),
        Some(row.current.as_ref().unwrap().oid.clone())
    );
}

#[test]
fn scenario_d2_symlink_repoint() {
    let mut repo = FixtureRepo::new("d2").unwrap();
    repo.symlink("f1", "link");
    repo.commit("add link").unwrap();
    let mut s = Fresh::over(repo, Config::default(), EngineOptions::default(), false);
    assert_pile!(s.engine, s.root, "");
    std::fs::remove_file(s.repo.path().join("link")).unwrap();
    s.repo.symlink("f2", "link");
    let pile = assert_pile!(
        s.engine,
        s.root,
        "link",
        "D2 repointed symlink pending, target untouched"
    );
    let row = pile.row(b"link").unwrap();
    assert_eq!(row.current.as_ref().unwrap().mode, Mode::Symlink);
    assert_eq!(row.change, Change::Modified);
    let bytes = s
        .store()
        .cat_blob(&row.current.as_ref().unwrap().oid)
        .unwrap();
    assert_eq!(bytes, b"f2", "the blob is the link target, not the pointee");
    assert!(s.accept_file("link").ok());
    assert_pile!(s.engine, s.root, "");
}

#[test]
fn scenario_d3_crlf_with_text_auto() {
    let mut repo = FixtureRepo::new("d3").unwrap();
    repo.commit_files(
        &[
            (".gitattributes", "* text=auto\n"),
            ("crlf.txt", "a\r\nb\r\n"),
        ],
        "crlf",
    )
    .unwrap();
    let mut s = Fresh::over(repo, Config::default(), EngineOptions::default(), false);
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D3 first sight clean despite CRLF on disk"
    );
    s.repo.write("crlf.txt", "a\r\nB\r\n");
    let pile = assert_pile!(s.engine, s.root, "crlf.txt", "D3 CRLF: pending");
    let row = pile.row(b"crlf.txt").unwrap();
    assert_eq!(
        (row.added, row.deleted),
        (1, 1),
        "one changed line, not a whole-file EOL churn"
    );
}

#[test]
fn scenario_d4_case_only_rename() {
    let mut s = Fresh::new("d4");
    if !probe_case_insensitive(&s.root) {
        eprintln!("D4 skipped: {} is case-sensitive", s.root.display());
        return;
    }
    std::fs::rename(s.repo.path().join("f1"), s.repo.path().join("F1")).unwrap();
    let pile = assert_pile!(
        s.engine,
        s.root,
        "F1|f1",
        "D4 case-only rename shows both sides"
    );
    assert_eq!(pile.row(b"f1").unwrap().change, Change::Deleted);
    assert_eq!(pile.row(b"F1").unwrap().change, Change::Added);
    let out = s.accept_all();
    assert!(out.ok(), "{out:?}");
    assert_pile!(s.engine, s.root, "");
}

#[test]
fn scenario_d5_unstaged_rename_with_edit() {
    let mut repo = FixtureRepo::new("d5").unwrap();
    repo.commit_files(
        &[("d/old.rs", "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n")],
        "old",
    )
    .unwrap();
    let mut s = Fresh::over(repo, Config::default(), EngineOptions::default(), false);
    s.repo.remove("d/old.rs");
    s.repo
        .write("d/new.rs", "l1\nL2\nl3\nl4\nl5\nl6\nl7\nl8\nL9\nl10\n");
    let pile = assert_pile!(s.engine, s.root, "d/new.rs|d/old.rs", "D5 rename with edit");
    let new = pile.row(b"d/new.rs").unwrap();
    let old = pile.row(b"d/old.rs").unwrap();
    assert_eq!(new.change, Change::Added);
    assert_eq!(old.change, Change::Deleted);
    match &new.rename {
        Some(Rename::From { from, similarity }) => {
            assert_eq!(from, b"d/old.rs");
            assert!(*similarity >= 50, "similarity {similarity}");
        }
        other => panic!("new.rs should be paired: {other:?}"),
    }
    assert!(
        matches!(&old.rename, Some(Rename::To { to, .. }) if to == b"d/new.rs"),
        "{:?}",
        old.rename
    );
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "");
}

#[test]
fn scenario_d6_sparse_checkout_is_not_deletion() {
    let mut repo = FixtureRepo::new("d6").unwrap();
    repo.commit_files(&[("src/a", "a\n"), ("other/o", "o\n")], "layout")
        .unwrap();
    let mut s = Fresh::over(repo, Config::default(), EngineOptions::default(), false);
    assert_pile!(s.engine, s.root, "");
    s.repo
        .git(&["sparse-checkout", "set", "--no-cone", "src"])
        .unwrap();
    if s.repo.path().join("other/o").exists() {
        eprintln!("D6 skipped: sparse-checkout did not remove other/o");
        return;
    }
    assert_pile!(s.engine, s.root, "", "D6 sparse: no false deletions");
    s.repo.write("src/a", "A\n");
    assert_pile!(
        s.engine,
        s.root,
        "src/a",
        "D6 sparse: real edits still pending"
    );
    // A skip-worktree path that is present (and modified) is a real file, not a cone.
    std::fs::create_dir_all(s.repo.path().join("other")).unwrap();
    s.repo.write("other/o", "O\n");
    let pile = s.engine.scan(&s.root).unwrap();
    assert!(
        pile.row(b"other/o").is_some() && pile.row(b"src/a").is_some(),
        "D6 sparse: a present skip-worktree edit is shown: {:?}",
        pile.rows.iter().map(|r| r.path.clone()).collect::<Vec<_>>()
    );
}

#[test]
fn scenario_d7_collapsed_glob() {
    let mut repo = FixtureRepo::new("d7").unwrap();
    repo.commit_files(&[("package-lock.json", "{\"v\":1}\n")], "lock")
        .unwrap();
    let mut s = Fresh::over(repo, Config::default(), EngineOptions::default(), false);
    s.repo.write("package-lock.json", "{\"v\":2}\n");
    let pile = assert_pile!(s.engine, s.root, "package-lock.json");
    let row = pile.row(b"package-lock.json").unwrap();
    assert_eq!(row.collapsed, Some(Collapsed::Glob));
    assert!(row.hunks.is_empty(), "collapsed rows carry no hunks");
    assert!(s.accept_file("package-lock.json").ok());
    assert_pile!(s.engine, s.root, "");
}

#[test]
fn scenario_d8_binary_and_oversize_collapse() {
    let mut repo = FixtureRepo::new("d8").unwrap();
    repo.commit_files(
        &[("img.bin", "\0\x01\x02\x03"), ("big.txt", "small\n")],
        "bin",
    )
    .unwrap();
    let config = Config {
        collapse_size_bytes: 100,
        ..Config::default()
    };
    let mut s = Fresh::over(repo, config, EngineOptions::default(), false);
    s.repo.write("img.bin", "\0\x01\x02\x03\x04");
    let big: String = (0..40).map(|i| format!("line {i}\n")).collect();
    assert!(big.len() > 100);
    s.repo.write("big.txt", big);
    let pile = assert_pile!(s.engine, s.root, "big.txt|img.bin");
    assert_eq!(
        pile.row(b"img.bin").unwrap().collapsed,
        Some(Collapsed::Binary)
    );
    assert_eq!(
        pile.row(b"big.txt").unwrap().collapsed,
        Some(Collapsed::Size)
    );
    assert!(pile.rows.iter().all(|r| r.hunks.is_empty()));
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "");
}

#[test]
fn scenario_d9_nested_repo_is_its_own_root() {
    let mut s = Fresh::new("d9");
    let nested = s.repo.path().join("vendor/lib");
    std::fs::create_dir_all(&nested).unwrap();
    s.repo.git_at(&nested, &["init", "-q"]).unwrap();
    s.repo.write("vendor/lib/x", "x\n");
    s.repo.git_at(&nested, &["add", "x"]).unwrap();
    s.repo
        .git_at(&nested, &["commit", "-qm", "nested"])
        .unwrap();
    // The outer scan discovers the nested repo; scan_all rescans to open it.
    let results = s.engine.scan_all();
    assert!(results.iter().all(|(_, r)| r.is_ok()), "{results:?}");
    let nested_canon = std::fs::canonicalize(&nested).unwrap();
    let root = s
        .engine
        .root(&nested_canon)
        .expect("nested repo became a root");
    assert_eq!(root.badge, Some(Badge::NestedIn(s.root.clone())));
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D9 outer pile excludes the nested repo"
    );
    assert_pile!(s.engine, nested_canon, "", "D9 nested first sight");
    s.repo.write("vendor/lib/x", "y\n");
    assert_pile!(s.engine, s.root, "", "D9 outer still excludes it");
    assert_pile!(
        s.engine,
        nested_canon,
        "x",
        "D9 nested edit pending in its own root"
    );
}

#[test]
fn scenario_d10_linked_worktree_commit_keeps_pending() {
    let repo = FixtureRepo::new("d10").unwrap();
    let wt = repo.parent_dir().join("d10.wt");
    repo.git(&["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wt"])
        .unwrap();
    let mut s = Fresh::over(repo, Config::default(), EngineOptions::default(), true);
    let wt = std::fs::canonicalize(&wt).unwrap();
    let root = s.engine.root(&wt).expect("linked worktree is a root");
    assert_eq!(root.badge, Some(Badge::WorktreeOf(s.root.clone())));
    assert_pile!(s.engine, wt, "");
    std::fs::write(
        wt.join("f1"),
        "a1\na2\na3\na4\na5\na6\na7\na8\na9\na10\ne\n",
    )
    .unwrap();
    assert_pile!(s.engine, wt, "f1");
    s.repo.git_at(&wt, &["commit", "-qam", "wt"]).unwrap();
    assert_pile!(
        s.engine,
        wt,
        "f1",
        "D10 linked worktree: commit keeps pending"
    );
    let notice = s
        .engine
        .inspect_head(&wt)
        .unwrap()
        .expect("HEAD moved in the worktree")
        .notice;
    assert_eq!(notice.as_deref(), Some("committed on wt (1 commit)"));
    assert_pile!(s.engine, s.root, "", "D10 main worktree unaffected");
}

#[test]
fn scenario_d11_unicode_and_space_path() {
    let mut s = Fresh::new("d11");
    s.repo.write("docs/résumé draft.md", "x\n");
    assert_pile!(
        s.engine,
        s.root,
        "docs/résumé draft.md",
        "D11 unicode+space path"
    );
    assert!(s.accept_file("docs/résumé draft.md").ok());
    assert_pile!(s.engine, s.root, "");
    assert!(s.ledger().overrides.contains_key("docs/résumé draft.md"));
    s.restart();
    assert_pile!(s.engine, s.root, "");
}
