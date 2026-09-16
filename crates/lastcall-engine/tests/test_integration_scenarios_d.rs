//! Scenario suite D — file-system and repo-shape edge cases (docs/spec/01-scenarios.md §D).

mod common;

use common::Fresh;
use lastcall_engine::config::Config;
use lastcall_engine::engine::{EngineOptions, RestoreRequest};
use lastcall_engine::git::Mode;
use lastcall_engine::headstate::{self, InProgress};
use lastcall_engine::ops::{NoFault, Refused, Rendered};
use lastcall_engine::roots::Badge;
use lastcall_engine::scan::{Change, Collapsed, Rename, probe_case_insensitive};
use lastcall_testkit::assert_pile;
use lastcall_testkit::engine::{open_engine, open_engine_with};
use lastcall_testkit::fixture_repo::{AUTHOR_EMAIL, AUTHOR_NAME, FixtureRepo, SEED_FILES};
use lastcall_testkit::tmp::TempDir;

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

/// D7 at the scenario's own size: `npm install` rewrites a 4,000-line lockfile. The row is
/// one accept, the counts are still live (thousands added and deleted), and nothing under
/// the frozen default `collapsed_globs` was overridden. Phase 6 gate item 2.
#[test]
fn scenario_d7_lockfile_churn_is_one_accept_row() {
    let before: String = (0..4_000)
        .map(|i| format!("    \"pkg-{i}\": {{ \"version\": \"1.0.{i}\" }},\n"))
        .collect();
    let mut repo = FixtureRepo::new("d7-churn").unwrap();
    repo.commit_files(&[("package-lock.json", before.as_str())], "lock")
        .unwrap();
    // The frozen defaults: no `collapsed_globs` or `collapse_size_bytes` override.
    let mut s = Fresh::over(repo, Config::default(), EngineOptions::default(), false);
    // `npm install`: most versions move, so most lines are rewritten in place.
    let after: String = (0..4_000)
        .map(|i| {
            if i % 3 == 0 {
                format!("    \"pkg-{i}\": {{ \"version\": \"1.0.{i}\" }},\n")
            } else {
                format!("    \"pkg-{i}\": {{ \"version\": \"2.4.{i}\" }},\n")
            }
        })
        .collect();
    s.repo.write("package-lock.json", &after);
    assert!(
        after.len() < 512 * 1024,
        "the lockfile is under collapse_size_bytes, so Glob is the reason: {}",
        after.len()
    );

    let pile = assert_pile!(s.engine, s.root, "package-lock.json");
    assert_eq!(pile.rows.len(), 1, "one row, not thousands of hunks");
    let row = pile.row(b"package-lock.json").unwrap();
    assert_eq!(row.collapsed, Some(Collapsed::Glob));
    assert!(row.hunks.is_empty(), "collapsed rows carry no hunks");
    assert!(
        row.added + row.deleted > 3_000,
        "the counts stay live on a collapsed row: +{} −{}",
        row.added,
        row.deleted
    );

    // One accept clears the whole row (§6.3 "single accept").
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

/// D8 at the **frozen default** `collapse_size_bytes` (512 KiB, no override): the 2 MB
/// binary and the 600 KiB generated file of the scenario, plus the boundary the code's
/// strict `>` defines — 524,288 bytes is not collapsed, 524,289 is. Phase 6 gate item 3.
#[test]
fn scenario_d8_binary_and_oversize_at_the_frozen_default() {
    const LIMIT: usize = 512 * 1024; // 524,288
    const BINARY_PROBE_WINDOW: usize = 8_000;

    // `line` is 32 bytes with its terminator, so LIMIT is a whole number of lines and
    // "one byte over" is a one-byte unterminated tail rather than a whole extra line.
    let line = |c: char| format!("{}\n", std::iter::repeat_n(c, 31).collect::<String>());
    let at_limit: String = std::iter::repeat_n(line('a'), LIMIT / 32).collect();
    assert_eq!(at_limit.len(), LIMIT);

    // A 2 MB PNG-shaped blob: the magic, then the IHDR length word whose NUL bytes land
    // inside the binary probe window (git's heuristic is a NUL in the first 8,000 bytes).
    let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    png.resize(2 * 1024 * 1024, b'\x42');
    assert!(
        png[..BINARY_PROBE_WINDOW].contains(&0),
        "NUL in the probe window"
    );
    // A 600 KiB generated file, text throughout.
    let generated: String = (0..31_000)
        .map(|i| format!("generated line {i}\n"))
        .collect();
    assert!(
        generated.len() > 600 * 1024 && generated.len() < 700 * 1024,
        "≈600 KiB of text: {}",
        generated.len()
    );

    let mut repo = FixtureRepo::new("d8-default").unwrap();
    repo.commit_files(
        &[
            ("img.png", "placeholder\n"),
            ("generated.txt", "seed\n"),
            ("at_limit.txt", at_limit.as_str()),
            ("over_limit.txt", at_limit.as_str()),
        ],
        "seed",
    )
    .unwrap();
    // No config override: this is what a user gets out of the box.
    let mut s = Fresh::over(repo, Config::default(), EngineOptions::default(), false);
    assert_eq!(s.engine.config().collapse_size_bytes, LIMIT as u64);

    s.repo.write("img.png", &png);
    s.repo.write("generated.txt", &generated);
    // Exactly at the limit on both sides: `>` is strict, so this is a normal hunk row.
    let mut changed_at_limit = at_limit.clone();
    changed_at_limit.replace_range(0..32, &line('b'));
    assert_eq!(changed_at_limit.len(), LIMIT);
    s.repo.write("at_limit.txt", &changed_at_limit);
    // One byte over on the new side.
    let over = format!("{at_limit}x");
    assert_eq!(over.len(), LIMIT + 1);
    s.repo.write("over_limit.txt", &over);

    let pile = assert_pile!(
        s.engine,
        s.root,
        "at_limit.txt|generated.txt|img.png|over_limit.txt"
    );
    assert_eq!(
        pile.row(b"img.png").unwrap().collapsed,
        Some(Collapsed::Binary),
        "a 2 MB file with a NUL in its first bytes is Binary"
    );
    assert_eq!(
        pile.row(b"generated.txt").unwrap().collapsed,
        Some(Collapsed::Size),
        "a 600 KiB text file is over the 512 KiB default"
    );
    assert_eq!(
        pile.row(b"at_limit.txt").unwrap().collapsed,
        None,
        "524,288 bytes is not over 524,288"
    );
    assert!(
        !pile.row(b"at_limit.txt").unwrap().hunks.is_empty(),
        "a row at the boundary keeps its hunks"
    );
    assert_eq!(
        pile.row(b"over_limit.txt").unwrap().collapsed,
        Some(Collapsed::Size),
        "524,289 bytes is over the limit"
    );
    for path in [&b"img.png"[..], b"generated.txt", b"over_limit.txt"] {
        let row = pile.row(path).unwrap();
        assert!(
            row.hunks.is_empty(),
            "{} is collapsed, so it carries no hunks",
            row.path_lossy()
        );
    }

    assert!(s.accept_all_snapshot(&pile).ok());
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
    assert!(results.iter().all(|(_, _, r)| r.is_ok()), "{results:?}");
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

// ---------------------------------------------------------------------------------------
// Restore (Phase 7 deliverable 1) — the D scenarios from the write side.
// ---------------------------------------------------------------------------------------

#[test]
fn scenario_d2_symlink_restore_never_writes_through_the_link() {
    let mut repo = FixtureRepo::new("d2-restore").unwrap();
    repo.symlink("f1", "link");
    repo.commit("add link").unwrap();
    let mut s = Fresh::over(repo, Config::default(), EngineOptions::default(), false);
    assert_pile!(s.engine, s.root, "");
    let pointee_before = s.bytes_at("f1");
    let mtime_before = std::fs::symlink_metadata(s.repo.path().join("f1"))
        .unwrap()
        .modified()
        .unwrap();

    // (a) The link is repointed and put back. §6.4: a symlink restore is unlink + symlink,
    // never a write through the link — so `f1` must be untouched in both bytes and mtime.
    std::fs::remove_file(s.repo.path().join("link")).unwrap();
    s.repo.symlink("f2", "link");
    assert_pile!(s.engine, s.root, "link");
    let out = s.restore_file("link");
    assert!(out.outcome.ok(), "{:?}", out.outcome);
    assert!(!out.outcome.written, "a restore never writes the ledger");
    assert_eq!(
        std::fs::read_link(s.repo.path().join("link")).unwrap(),
        std::path::Path::new("f1"),
        "the link points back at its baseline target"
    );
    assert_eq!(s.bytes_at("f1"), pointee_before, "the pointee is untouched");
    assert_eq!(
        std::fs::symlink_metadata(s.repo.path().join("f1"))
            .unwrap()
            .modified()
            .unwrap(),
        mtime_before,
        "the pointee was never opened for writing"
    );
    assert_pile!(s.engine, s.root, "", "D2 restore clears the row");

    // (b) The other direction (F10): the baseline is a regular file and the live side is a
    // symlink pointing at another file. `rename` replaces the link itself; the file the
    // link pointed at is not written through.
    let decoy_before = s.bytes_at("f2");
    let f3_baseline = s.bytes_at("f3");
    std::fs::remove_file(s.repo.path().join("f3")).unwrap();
    s.repo.symlink("f2", "f3");
    assert_pile!(s.engine, s.root, "f3");
    let out = s.restore_file("f3");
    assert!(out.outcome.ok(), "{:?}", out.outcome);
    let meta = std::fs::symlink_metadata(s.repo.path().join("f3")).unwrap();
    assert!(
        meta.file_type().is_file(),
        "the link was replaced by the baseline file, not followed"
    );
    assert_eq!(s.bytes_at("f3"), f3_baseline);
    assert_eq!(s.bytes_at("f2"), decoy_before, "the pointee is untouched");
    assert_pile!(s.engine, s.root, "");
}

#[test]
fn scenario_d3_crlf_restore_keeps_crlf() {
    let mut repo = FixtureRepo::new("d3-restore").unwrap();
    // `eol=crlf` and not bare `text=auto`: on a native-LF platform `text=auto` alone makes
    // git's own checkout write LF, so a restore that wrote the canonical blob would be
    // *correct* there and the test would prove nothing. `eol=crlf` is the case F2 is about
    // — the worktree representation and the blob genuinely differ.
    repo.commit_files(
        &[
            (".gitattributes", "* text=auto eol=crlf\n"),
            ("crlf.txt", "a\r\nb\r\nc\r\n"),
        ],
        "crlf",
    )
    .unwrap();
    let mut s = Fresh::over(repo, Config::default(), EngineOptions::default(), false);
    assert_pile!(s.engine, s.root, "");
    let original = s.bytes_at("crlf.txt");
    assert_eq!(original, b"a\r\nb\r\nc\r\n");

    s.repo.write("crlf.txt", "a\r\nB\r\nc\r\n");
    let row = s.row("crlf.txt");
    assert_eq!(row.hunks.len(), 1);
    assert_eq!(
        s.store()
            .cat_blob(&row.baseline.as_ref().unwrap().oid)
            .unwrap(),
        b"a\nb\nc\n",
        "the store holds the canonical LF blob, not the worktree's bytes"
    );
    let out = s.restore_hunk("crlf.txt", 0);
    assert!(out.outcome.ok(), "{:?}", out.outcome);
    // F2: the store's blob is the *canonical* (LF) content, because `hash-object -w` ran
    // with cwd = root and `text=auto` cleaned it. Writing that blob raw would silently
    // convert the user's file to LF; `cat-file --filters` puts the CRLF back.
    assert_eq!(
        s.bytes_at("crlf.txt"),
        original,
        "the restored file is byte-equal to the CRLF original"
    );
    assert_pile!(s.engine, s.root, "", "D3 restore clears the row");
}

/// The D3 case `scenario_d3_crlf_restore_keeps_crlf` deliberately steps around: **bare**
/// `* text=auto`, where the worktree representation is *not* reproducible from the blob.
///
/// Git cleans the user's CRLF file to an LF blob on the way in and writes LF on the way
/// out, so `cat-file --filters` cannot put the CRLF back. Restoring one hunk used to
/// rewrite all three line endings and then report a clean pile, so the damage was invisible
/// as well as unasked-for. The round-trip guard refuses instead (verifier F2, ruled option
/// (a)): restore never normalises line endings behind the user's back.
#[test]
fn scenario_d3_crlf_restore_under_bare_text_auto_is_refused() {
    let mut repo = FixtureRepo::new("d3-restore-bare").unwrap();
    repo.commit_files(
        &[
            (".gitattributes", "* text=auto\n"),
            ("crlf.txt", "a\r\nb\r\nc\r\n"),
        ],
        "crlf",
    )
    .unwrap();
    let mut s = Fresh::over(repo, Config::default(), EngineOptions::default(), false);
    let original = s.bytes_at("crlf.txt");
    assert_eq!(original, b"a\r\nb\r\nc\r\n", "the worktree keeps CRLF");

    let edited = "a\r\nB\r\nc\r\n";
    s.repo.write("crlf.txt", edited);
    let pile_before = s.scan();
    let row = s.row("crlf.txt");
    assert_eq!(row.hunks.len(), 1, "one changed line");

    let out = s.restore_hunk("crlf.txt", 0);
    match out.outcome.refused.first() {
        // Verifier (b) F7: the refusal is its own variant, and its message does not claim
        // the file could not be hashed — hashing it is how the guard knows.
        Some(r @ Refused::NotRoundTrippable { .. }) => {
            assert_eq!(
                r.message("restored"),
                "crlf.txt: eol conversion is not round-trippable; not restored"
            );
        }
        other => panic!("expected a round-trip refusal, got {other:?}"),
    }
    assert_eq!(
        s.bytes_at("crlf.txt"),
        edited.as_bytes(),
        "the user's bytes are untouched — including the two endings the restore would have rewritten"
    );
    assert_eq!(
        common::pile_string(&s.scan()),
        common::pile_string(&pile_before),
        "the pile is unchanged: nothing was hidden"
    );
}

#[test]
fn scenario_d4_restore_deletion_refuses_on_a_case_collision() {
    let mut s = Fresh::new("d4-restore");
    if !probe_case_insensitive(&s.root) {
        eprintln!("D4 restore skipped: {} is case-sensitive", s.root.display());
        return;
    }
    let original = s.bytes_at("f1");
    std::fs::rename(s.repo.path().join("f1"), s.repo.path().join("F1")).unwrap();
    assert_pile!(s.engine, s.root, "F1|f1", "D4 case-only rename");
    assert_eq!(s.row("f1").change, Change::Deleted);

    // F6: `f1` looks absent to `stat` only because APFS folds; `read_dir` shows `F1` right
    // there. A byte-only absence rule would let `rename(temp, "f1")` fold onto `F1` and
    // destroy the user's case-only rename.
    let out = s.restore_file("f1");
    match out.outcome.refused.first() {
        Some(r @ Refused::StillPresent { .. }) => {
            assert!(
                r.message("restored").contains("F1"),
                "the colliding entry is named: {}",
                r.message("restored")
            );
        }
        other => panic!("expected a case-collision refusal, got {other:?}"),
    }
    assert_eq!(s.bytes_at("F1"), original, "F1 is untouched");
    assert_pile!(s.engine, s.root, "F1|f1", "both sides still pending");
}

/// The same clobber D4 guards against, one code point past ASCII (verifier F1).
///
/// The old rule folded with `eq_ignore_ascii_case`, which says `école.md` and `École.md`
/// are different names. APFS says they are the same one, so the rename landed on the
/// user's renamed file and replaced its content with the baseline. The fold question now
/// goes to the filesystem, so this refuses.
#[test]
fn scenario_d4_restore_deletion_refuses_on_a_unicode_case_collision() {
    let mut s = Fresh::new("d4-restore-unicode");
    if !probe_case_insensitive(&s.root) {
        eprintln!(
            "SKIP scenario_d4_restore_deletion_refuses_on_a_unicode_case_collision: {} is case-sensitive",
            s.root.display()
        );
        return;
    }
    let lower = "école.md";
    let upper = "École.md";
    s.repo.write(lower, "one\n");
    assert!(
        s.accept_file(lower).ok(),
        "the baseline is the accepted blob"
    );
    assert_pile!(s.engine, s.root, "");

    std::fs::rename(s.repo.path().join(lower), s.repo.path().join(upper)).unwrap();
    let newer = b"the user's newer content\n";
    std::fs::write(s.repo.path().join(upper), newer).unwrap();
    assert_eq!(s.row(lower).change, Change::Deleted);

    let out = s.restore_file(lower);
    match out.outcome.refused.first() {
        Some(r @ Refused::StillPresent { collides_with, .. }) => {
            assert_eq!(
                collides_with.as_deref(),
                Some(upper.as_bytes()),
                "the colliding entry is named by its real bytes"
            );
            assert!(
                r.message("restored").contains(upper),
                "the message names it: {}",
                r.message("restored")
            );
        }
        other => panic!("expected a Unicode case-collision refusal, got {other:?}"),
    }
    assert_eq!(
        s.bytes_at(upper),
        newer,
        "the newer bytes survive — this is the data loss the ASCII fold allowed"
    );
}

/// APFS is normalization-*insensitive* as well as case-insensitive: `café.md` written NFD
/// and read back NFC is one file, so the same filesystem-answers-it rule has to cover
/// normalization and not only case (verifier F1).
///
/// **A git root cannot reach this through a row**, and the test says so with evidence:
/// `core.precomposeunicode` is on by default on macOS, so git precomposes every path it
/// reports and lastcall only ever sees the NFC name — there is no NFD row to restore. The
/// guard still has to hold, because [`lastcall_engine::restore::collision`] is what stands
/// between `rename(temp, name)` and the user's file on **every** root, including a draft
/// root, whose raw-byte content model has no git in front of it to normalize anything. So
/// the rule is asserted directly, over a real working directory on the real filesystem.
#[test]
fn scenario_d4_restore_deletion_refuses_on_an_nfd_nfc_collision() {
    let mut s = Fresh::new("d4-restore-nfd");
    if !probe_case_insensitive(&s.root) {
        eprintln!(
            "SKIP scenario_d4_restore_deletion_refuses_on_an_nfd_nfc_collision: {} is case-sensitive",
            s.root.display()
        );
        return;
    }
    // `e` + U+0301 (decomposed) vs the precomposed `é`. Different bytes, one grapheme.
    let nfd = "cafe\u{0301}.md";
    let nfc = "caf\u{00e9}.md";
    assert_ne!(nfd.as_bytes(), nfc.as_bytes());

    // The evidence for the paragraph above: created with the decomposed name on disk,
    // reported by the scan under the precomposed one.
    s.repo.write(nfd, "one\n");
    assert_eq!(
        s.row(nfc).path,
        nfc.as_bytes(),
        "git precomposes: the NFD name on disk is reported NFC, so no NFD row exists"
    );
    s.repo.remove(nfd);
    assert_pile!(s.engine, s.root, "");

    // Now the shape the guard exists for: the file on disk carries one normalization and
    // the *other* name resolves onto it. This is the state in which a deletion restore
    // must not `rename` over the user's file.
    let newer = b"the user's newer content\n";
    s.repo.write(nfc, newer);
    let collides = lastcall_engine::restore::collision(&s.root, nfd.as_bytes(), true);
    assert_eq!(
        collides.as_deref(),
        Some(nfc.as_bytes()),
        "the NFD name is taken, and the precomposed entry is what is in the way"
    );
    // The byte-exact half is unconditional, and the negative case still says no.
    assert_eq!(
        lastcall_engine::restore::collision(&s.root, nfc.as_bytes(), true).as_deref(),
        Some(nfc.as_bytes())
    );
    assert_eq!(
        lastcall_engine::restore::collision(&s.root, "unrelated.md".as_bytes(), true),
        None
    );
    assert_eq!(s.bytes_at(nfc), newer, "nothing was written over it");
}

#[test]
fn scenario_d11_restore_of_a_unicode_and_space_path() {
    let mut s = Fresh::new("d11-restore");
    let path = "docs/résumé draft.md";
    s.repo.write(path, "one\ntwo\nthree\n");
    assert!(s.accept_file(path).ok());
    assert_pile!(s.engine, s.root, "");
    s.repo.write(path, "one\nTWO\nthree\n");
    assert_pile!(s.engine, s.root, path);
    let out = s.restore_file(path);
    assert!(out.outcome.ok(), "{:?}", out.outcome);
    assert_eq!(s.bytes_at(path), b"one\ntwo\nthree\n");
    assert_pile!(s.engine, s.root, "", "D11 restore clears the row");

    // And the deletion direction: the whole directory goes, and the restore rebuilds it.
    s.repo.remove(path);
    std::fs::remove_dir(s.repo.path().join("docs")).unwrap();
    assert_pile!(s.engine, s.root, path);
    let out = s.restore_file(path);
    assert!(out.outcome.ok(), "{:?}", out.outcome);
    assert_eq!(s.bytes_at(path), b"one\ntwo\nthree\n");
    assert_pile!(s.engine, s.root, "");
}

// ---- D12 and D13: search_depth (Amendment v1.11, deliverable 8) ----

/// Every root path the engine lists, relative to the parent dir, sorted by bytes.
fn listed(engine: &lastcall_engine::engine::Engine, parent: &std::path::Path) -> Vec<String> {
    engine
        .roots()
        .iter()
        .map(|r| {
            r.path
                .strip_prefix(parent)
                .unwrap_or(&r.path)
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn at_depth(
    parent: &std::path::Path,
    env: &lastcall_engine::env::Env,
    state: &std::path::Path,
    depth: u8,
) -> lastcall_engine::engine::Engine {
    open_engine(
        parent,
        env,
        state,
        Config {
            search_depth: depth,
            ..Config::default()
        },
    )
}

#[test]
fn scenario_d12_search_depth_reads_n_folders_down() {
    // Setup, line by line from D12: P/a (repo), P/a/sub (a committed submodule of a),
    // P/worktrees/b (a clone of a), P/worktrees/a-wt (a linked worktree of a),
    // P/deep/er/c, P/node_modules/pkg, P/link -> P/deep.
    let repo = FixtureRepo::new("a").unwrap();
    let parent = std::fs::canonicalize(repo.parent_dir()).unwrap();
    let a = std::fs::canonicalize(repo.path()).unwrap();

    let sub = repo.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    repo.git_at(&sub, &["init", "-q", "-b", "main"]).unwrap();
    std::fs::write(sub.join("s"), "s\n").unwrap();
    repo.git_at(&sub, &["add", "s"]).unwrap();
    repo.git_at(&sub, &["commit", "-qm", "sub"]).unwrap();
    repo.git(&["add", "sub"]).unwrap();
    repo.git(&["commit", "-qm", "gitlink"]).unwrap();

    let worktrees = parent.join("worktrees");
    std::fs::create_dir_all(&worktrees).unwrap();
    let b = worktrees.join("b");
    repo.git(&["clone", "-q", a.to_str().unwrap(), b.to_str().unwrap()])
        .unwrap();
    let a_wt = worktrees.join("a-wt");
    repo.git(&[
        "worktree",
        "add",
        "-q",
        a_wt.to_str().unwrap(),
        "-b",
        "feat-w",
    ])
    .unwrap();

    let c = parent.join("deep/er/c");
    std::fs::create_dir_all(&c).unwrap();
    repo.git_at(&c, &["init", "-q", "-b", "main"]).unwrap();
    let pkg = parent.join("node_modules/pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    repo.git_at(&pkg, &["init", "-q", "-b", "main"]).unwrap();
    std::os::unix::fs::symlink(parent.join("deep"), parent.join("link")).unwrap();

    let state = TempDir::new("lc-d12-state");
    let env = repo.engine_env(state.path());
    let never = ["a/sub", "node_modules/pkg", "link/er/c"];

    // search_depth = 1: the repositories directly inside P, which is what every release
    // before Amendment v1.11 listed.
    let engine = at_depth(&parent, &env, state.path(), 1);
    assert_eq!(listed(&engine, &parent), vec!["a"], "D12 depth 1");
    drop(engine);
    let engine = at_depth(&parent, &env, state.path(), 1);
    assert_eq!(listed(&engine, &parent), vec!["a"], "D12 depth 1 restart");
    drop(engine);

    // search_depth = 2: one plain folder further, with the badges and the parents.
    let mut engine = at_depth(&parent, &env, state.path(), 2);
    assert_eq!(
        listed(&engine, &parent),
        vec!["a", "worktrees/a-wt", "worktrees/b"],
        "D12 depth 2"
    );
    for root in engine.roots() {
        assert_eq!(
            root.parent,
            parent,
            "{} is filed under P",
            root.path.display()
        );
    }
    assert_eq!(
        engine.root(&a_wt).unwrap().badge,
        Some(Badge::WorktreeOf(a.clone())),
        "D12 the linked worktree keeps its badge one folder down"
    );
    assert_eq!(
        engine.root(&b).unwrap().badge,
        None,
        "D12 a clone is a clone"
    );
    // The ledgers that must still be on disk after the depth goes back down.
    let kept: Vec<std::path::PathBuf> = [&a_wt, &b]
        .iter()
        .map(|p| engine.root(p).unwrap().paths.ledger.clone())
        .collect();
    // The pile of each new root is its own, and the submodule is nobody's root.
    assert_pile!(engine, b, "", "D12 the clone's first sight");
    assert_pile!(engine, a_wt, "", "D12 the linked worktree's first sight");
    for name in never {
        assert!(
            engine.root(&parent.join(name)).is_none(),
            "D12 depth 2 never lists {name}"
        );
    }
    drop(engine);
    let engine = at_depth(&parent, &env, state.path(), 2);
    assert_eq!(
        listed(&engine, &parent),
        vec!["a", "worktrees/a-wt", "worktrees/b"],
        "D12 depth 2 restart"
    );
    drop(engine);

    // search_depth = 3: two folders further.
    let engine = at_depth(&parent, &env, state.path(), 3);
    assert_eq!(
        listed(&engine, &parent),
        vec!["a", "deep/er/c", "worktrees/a-wt", "worktrees/b"],
        "D12 depth 3"
    );
    for name in never {
        assert!(
            engine.root(&parent.join(name)).is_none(),
            "D12 depth 3 never lists {name}"
        );
    }
    drop(engine);
    let engine = at_depth(&parent, &env, state.path(), 3);
    assert_eq!(
        listed(&engine, &parent),
        vec!["a", "deep/er/c", "worktrees/a-wt", "worktrees/b"],
        "D12 depth 3 restart"
    );
    drop(engine);

    // Back to 1: `a` only, and the ledgers the deeper roots wrote are still on disk.
    let engine = at_depth(&parent, &env, state.path(), 1);
    assert_eq!(listed(&engine, &parent), vec!["a"], "D12 back to depth 1");
    for ledger in &kept {
        assert!(
            ledger.is_file(),
            "D12 {} survives the depth drop",
            ledger.display()
        );
    }
}

#[test]
fn scenario_d13_worktree_kept_inside_its_repository() {
    // Setup: P/R with `.worktrees/` in its committed .gitignore, and a linked worktree
    // inside it. Mechanism 1 cannot see it (the walk stops at a repository); mechanism 2
    // is what lists it.
    let mut repo = FixtureRepo::new("R").unwrap();
    repo.commit_files(&[(".gitignore", ".worktrees/\n")], "ignore worktrees")
        .unwrap();
    repo.git(&["worktree", "add", "-q", ".worktrees/wt", "-b", "feat-w"])
        .unwrap();
    let parent = std::fs::canonicalize(repo.parent_dir()).unwrap();
    let r = std::fs::canonicalize(repo.path()).unwrap();
    let wt = std::fs::canonicalize(repo.path().join(".worktrees/wt")).unwrap();

    let state = TempDir::new("lc-d13-state");
    let env = repo.engine_env(state.path());

    // search_depth = 1: R alone. The worktree is ignored, so D9's nested path does not
    // report it either.
    let engine = at_depth(&parent, &env, state.path(), 1);
    assert_eq!(listed(&engine, &parent), vec!["R"], "D13 depth 1");
    drop(engine);

    // search_depth = 2: both, filed under P, the worktree badged.
    let mut engine = at_depth(&parent, &env, state.path(), 2);
    assert_eq!(
        listed(&engine, &parent),
        vec!["R", "R/.worktrees/wt"],
        "D13 depth 2"
    );
    for root in engine.roots() {
        assert_eq!(
            root.parent,
            parent,
            "{} is filed under P",
            root.path.display()
        );
    }
    assert_eq!(
        engine.root(&wt).unwrap().badge,
        Some(Badge::WorktreeOf(r.clone())),
        "D13 worktree of R"
    );
    assert_pile!(engine, r, "", "D13 R holds nothing under .worktrees/");
    // D10 in the new shape: an agent edit inside the worktree is pending there only.
    std::fs::write(
        wt.join("f1"),
        "a1\na2\na3\na4\na5\na6\na7\na8\na9\na10\ne\n",
    )
    .unwrap();
    assert_pile!(engine, wt, "f1", "D13 the edit is pending in wt");
    assert_pile!(engine, r, "", "D13 and nowhere else");
    drop(engine);
    let engine = at_depth(&parent, &env, state.path(), 2);
    assert_eq!(
        listed(&engine, &parent),
        vec!["R", "R/.worktrees/wt"],
        "D13 depth 2 restart"
    );
    drop(engine);

    // Launched from inside R: the parent dir is itself a repository, so nothing is walked
    // under it, and mechanism 2 still lists the worktree kept inside it.
    let inside = TempDir::new("lc-d13-inside");
    let inside_env = repo.engine_env(inside.path());
    let engine = at_depth(&r, &inside_env, inside.path(), 2);
    assert_eq!(
        listed(&engine, &r),
        vec!["", ".worktrees/wt"],
        "D13 launched inside R"
    );
    assert_eq!(
        engine.root(&wt).unwrap().badge,
        Some(Badge::WorktreeOf(r.clone()))
    );
    drop(engine);

    // A `.worktrees/` that is not ignored is listed at depth 1 already, through D9's
    // nested path, with the same badge.
    let mut open = FixtureRepo::new("R2").unwrap();
    open.commit_files(&[("keep", "keep\n")], "no ignore")
        .unwrap();
    open.git(&["worktree", "add", "-q", ".worktrees/wt", "-b", "feat-w"])
        .unwrap();
    let open_parent = std::fs::canonicalize(open.parent_dir()).unwrap();
    let open_r = std::fs::canonicalize(open.path()).unwrap();
    let open_wt = std::fs::canonicalize(open.path().join(".worktrees/wt")).unwrap();
    let open_state = TempDir::new("lc-d13-open-state");
    let open_env = open.engine_env(open_state.path());
    let mut engine = at_depth(&open_parent, &open_env, open_state.path(), 1);
    let results = engine.scan_all();
    assert!(results.iter().all(|(_, _, r)| r.is_ok()), "{results:?}");
    assert_eq!(
        listed(&engine, &open_parent),
        vec!["R2", "R2/.worktrees/wt"],
        "D13 a .worktrees/ that is not ignored is a nested root at depth 1"
    );
    assert_eq!(
        engine.root(&open_wt).unwrap().badge,
        Some(Badge::WorktreeOf(open_r)),
        "D13 the worktree badge wins over NestedIn"
    );
}

// ---------------------------------------------------------------------------------------
// Per-branch seen records (D14 to D23, Amendment v1.12). One record per branch the repo has
// been checked out on while lastcall watched; the record in force is the one the branch
// name in <git_dir>/HEAD selects.
// ---------------------------------------------------------------------------------------

/// The branch the record in force belongs to, read from the ledger's own JSON (the shape
/// the next process loads).
fn seen_branch(s: &Fresh) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(&s.ledger().to_json()).expect("ledger json");
    v.get("seen_branch")
        .and_then(|b| b.as_str())
        .map(str::to_owned)
}

/// The parked branch names, sorted (the `branches` map's keys; empty when the field is
/// omitted).
fn parked(s: &Fresh) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(&s.ledger().to_json()).expect("ledger json");
    match v.get("branches") {
        Some(serde_json::Value::Object(m)) => m.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

/// The root's `ledger.json` under the state dir, found by its `root` field the way a second
/// process finds it.
fn ledger_file(s: &Fresh) -> std::path::PathBuf {
    let want = s.root.to_string_lossy().into_owned();
    let roots = s.state.path().join("roots");
    let mut found = Vec::new();
    for parent in std::fs::read_dir(&roots).expect("state/roots").flatten() {
        let Ok(repos) = std::fs::read_dir(parent.path().join("repos")) else {
            continue;
        };
        for repo in repos.flatten() {
            let path = repo.path().join("ledger.json");
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let v: serde_json::Value = serde_json::from_str(&text).expect("ledger json");
            if v["root"].as_str() == Some(want.as_str()) {
                found.push(path);
            }
        }
    }
    assert_eq!(found.len(), 1, "exactly one ledger for {want}: {found:?}");
    found.pop().unwrap()
}

/// D15's fixture up to "three records": first sight on `main`, a run branch `run-1` with
/// `a.rs`/`b.rs` committed on it, a second run `run-2` with `c.rs`, and `main` in force at
/// the end with both runs parked.
fn d15_three_records(name: &str) -> Fresh {
    let mut s = Fresh::new(name);
    assert_pile!(s.engine, s.root, "", "D15 first sight on main");
    s.repo.checkout_b("run-1").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D15 run-1 starts as a copy of main's record"
    );
    s.repo.write("a.rs", "a\n");
    s.repo.write("b.rs", "b\n");
    s.repo.commit("run-1 work").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "a.rs|b.rs",
        "D15 the run's work is pending"
    );
    let _ = s.engine.inspect_head(&s.root).unwrap();
    s.repo.checkout("main").unwrap();
    assert_pile!(s.engine, s.root, "", "D15 back on main");
    s.repo.checkout_b("run-2").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D15 run-2 starts as a copy of main's record"
    );
    s.repo.write("c.rs", "c\n");
    s.repo.commit("run-2 work").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "c.rs",
        "D15 the second run's work is pending"
    );
    let _ = s.engine.inspect_head(&s.root).unwrap();
    s.repo.checkout("main").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D15 back on main after the second run"
    );
    assert_eq!(seen_branch(&s).as_deref(), Some("main"));
    assert_eq!(parked(&s), vec!["run-1".to_string(), "run-2".to_string()]);
    s
}

#[test]
fn scenario_d14_accepted_on_a_branch_then_the_start_branch_checked_out() {
    let repo = FixtureRepo::new("d14").unwrap();
    repo.checkout_b("future").unwrap(); // first sight happens here, on future
    let mut s = Fresh::over(repo, Config::default(), EngineOptions::default(), false);
    assert_eq!(seen_branch(&s).as_deref(), Some("future"));
    assert!(parked(&s).is_empty(), "no record for main");
    for i in 1..=4 {
        s.repo.write(&format!("n{i}"), format!("n{i}\n"));
    }
    s.repo.commit("agent adds four files").unwrap();
    for i in 1..=4 {
        assert!(s.accept_file(&format!("n{i}")).ok());
    }
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D14 the four files accepted on future"
    );
    assert_eq!(s.ledger().overrides.len(), 4, "four overrides on future");
    let _ = s.engine.inspect_head(&s.root).unwrap();
    s.restart();
    assert_pile!(s.engine, s.root, "", "D14 restart on future");

    s.repo.checkout("main").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D14 checkout main: the fold takes main's content, no deletions"
    );
    assert_eq!(
        s.head_notice().as_deref(),
        Some(
            "switched future → main: first time here, seen state carried from future; 0 files pending"
        )
    );
    assert_eq!(seen_branch(&s).as_deref(), Some("main"));
    assert_eq!(parked(&s), vec!["future".to_string()]);
    assert!(
        s.ledger().overrides.is_empty(),
        "the fold dropped the four overrides"
    );
    s.restart();
    assert_pile!(s.engine, s.root, "", "D14 restart on main");

    s.repo.checkout("future").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D14 back on future: the parked record"
    );
    assert_eq!(
        s.head_notice().as_deref(),
        Some("switched main → future: 0 files differ from seen state")
    );
    assert_eq!(s.ledger().overrides.len(), 4, "future's overrides are back");
    s.restart();
    assert_pile!(s.engine, s.root, "", "D14 restart back on future");
}

#[test]
fn scenario_d14_variant_first_sight_on_main_then_the_branch() {
    let mut s = Fresh::new("d14v");
    assert_pile!(s.engine, s.root, "", "D14 variant first sight on main");
    s.repo.checkout_b("future").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D14 variant at the branch creation: main parked, a copy in force"
    );
    assert_eq!(seen_branch(&s).as_deref(), Some("future"));
    assert_eq!(parked(&s), vec!["main".to_string()]);
    for i in 1..=4 {
        s.repo.write(&format!("n{i}"), format!("n{i}\n"));
    }
    s.repo.commit("agent adds four files").unwrap();
    for i in 1..=4 {
        assert!(s.accept_file(&format!("n{i}")).ok());
    }
    assert_pile!(s.engine, s.root, "", "D14 variant accepted on future");
    s.repo.checkout("main").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D14 variant back on main: no deletions"
    );
    s.repo.checkout("future").unwrap();
    assert_pile!(s.engine, s.root, "", "D14 variant back on future");
    s.restart();
    assert_pile!(s.engine, s.root, "", "D14 variant restart");
}

#[test]
fn scenario_d15_an_unattended_run_on_a_generated_branch() {
    let mut s = Fresh::new("d15");
    assert_pile!(s.engine, s.root, "", "D15 first sight on main");
    s.repo.checkout_b("run-1").unwrap();
    assert_pile!(s.engine, s.root, "", "D15 the copy at the branch creation");
    assert_eq!(
        s.head_notice().as_deref(),
        Some("switched main → run-1 (same commit)")
    );
    s.repo.write("a.rs", "a\n");
    s.repo.write("b.rs", "b\n");
    s.repo.commit("run-1 work").unwrap();
    s.repo.write("scratch.tmp", "scratch\n");
    assert_pile!(
        s.engine,
        s.root,
        "a.rs|b.rs|scratch.tmp",
        "D15 run-1 after the writes"
    );
    let _ = s.engine.inspect_head(&s.root).unwrap();
    s.repo.git(&["checkout", "-q", "."]).unwrap();
    s.repo.git(&["clean", "-qfd"]).unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "a.rs|b.rs",
        "D15 after the clean: the committed work waits"
    );
    s.restart();
    assert_pile!(s.engine, s.root, "a.rs|b.rs", "D15 restart on run-1");
    s.repo.checkout("main").unwrap();
    assert_pile!(s.engine, s.root, "", "D15 back on main");
    assert_eq!(
        s.head_notice().as_deref(),
        Some("switched run-1 → main: 0 files differ from seen state")
    );
    s.repo.checkout_b("run-2").unwrap();
    s.repo.write("c.rs", "c\n");
    s.repo.commit("run-2 work").unwrap();
    s.repo.git(&["checkout", "-q", "."]).unwrap();
    s.repo.git(&["clean", "-qfd"]).unwrap();
    s.repo.checkout("main").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D15 back on main after the second run"
    );
    s.repo.checkout("run-1").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "a.rs|b.rs",
        "D15 run-1's parked record: the run's work waiting for review"
    );
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "", "D15 accept-all on run-1");
    s.repo.checkout("main").unwrap();
    assert_pile!(s.engine, s.root, "", "D15 main after the review");
    s.repo.checkout("run-2").unwrap();
    assert_pile!(s.engine, s.root, "c.rs", "D15 run-2's work");
    assert_eq!(seen_branch(&s).as_deref(), Some("run-2"));
    assert_eq!(
        parked(&s),
        vec!["main".to_string(), "run-1".to_string()],
        "three records: run-2 in force and two parked"
    );
    s.restart();
    assert_pile!(s.engine, s.root, "c.rs", "D15 restart on run-2");
}

#[test]
fn scenario_d16_cherry_picks_show_once_more_by_design() {
    let mut s = d15_three_records("d16");
    s.repo.checkout("run-1").unwrap();
    assert_pile!(s.engine, s.root, "a.rs|b.rs");
    assert!(s.accept_all().ok());
    s.repo.checkout("run-2").unwrap();
    assert_pile!(s.engine, s.root, "c.rs");
    assert!(s.accept_all().ok());
    s.repo.checkout("main").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D16 main after both runs were accepted"
    );
    s.repo.checkout_b("feat/x").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D16 feat/x is a copy of main's record"
    );
    s.repo.git(&["cherry-pick", "main..run-1"]).unwrap();
    s.repo.git(&["cherry-pick", "main..run-2"]).unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "a.rs|b.rs|c.rs",
        "D16 the cherry-picked content shows again"
    );
    let before = s.ledger().seen_tree.clone();
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "", "D16 accept-all on feat/x");
    assert_ne!(s.ledger().seen_tree, before, "feat/x's seen_tree is a fold");
    s.repo.checkout("main").unwrap();
    assert_pile!(s.engine, s.root, "", "D16 main's record is untouched");
    s.restart();
    assert_pile!(s.engine, s.root, "", "D16 restart on main");
}

#[test]
fn scenario_d17_the_run_in_a_linked_worktree() {
    let repo = FixtureRepo::new("d17").unwrap();
    let wt = repo.parent_dir().join("d17-run");
    repo.git(&["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "run-3"])
        .unwrap();
    let mut s = Fresh::over(repo, Config::default(), EngineOptions::default(), true);
    let wt = std::fs::canonicalize(&wt).unwrap();
    assert_pile!(s.engine, wt, "", "D17 the worktree is its own root");
    let wt_ledger: serde_json::Value =
        serde_json::from_str(&s.engine.root(&wt).unwrap().ledger.to_json()).unwrap();
    assert_eq!(
        wt_ledger["seen_branch"].as_str(),
        Some("run-3"),
        "the worktree's own first sight, on its own branch"
    );
    assert!(parked(&s).is_empty(), "R's ledger has no run-3 record");
    let before = s.ledger().to_json();

    std::fs::write(wt.join("w1"), "w1\n").unwrap();
    s.repo.git_at(&wt, &["add", "-A"]).unwrap();
    s.repo
        .git_at(&wt, &["commit", "-qm", "run-3 work"])
        .unwrap();
    let pile = assert_pile!(s.engine, wt, "w1", "D17 the run's work, reviewed there");
    s.engine
        .ops(&wt)
        .unwrap()
        .accept_all(&pile, &NoFault)
        .unwrap();
    assert_pile!(
        s.engine,
        wt,
        "",
        "D17 accepted in the worktree's own ledger"
    );
    assert_pile!(s.engine, s.root, "", "D17 R's pile is empty throughout");

    s.repo.git_at(&wt, &["checkout", "-q", "--detach"]).unwrap();
    s.repo
        .git(&["worktree", "remove", wt.to_str().unwrap()])
        .unwrap();
    assert_pile!(s.engine, s.root, "", "D17 R after the worktree is removed");
    assert_eq!(s.ledger().to_json(), before, "R's ledger is unchanged");
    s.repo.checkout("run-3").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "w1",
        "D17 R first-sights run-3 from main's record: ahead, no fold, over-show"
    );
}

#[test]
fn scenario_d18_a_branch_that_is_ahead_never_seen() {
    let mut s = Fresh::new("d18");
    assert_pile!(s.engine, s.root, "", "D18 first sight on main");
    // feat/other exists locally and was never checked out while lastcall watched.
    s.repo.checkout_b("feat/other").unwrap();
    s.repo.write("o1", "o1\n");
    s.repo.commit("o1").unwrap();
    s.repo.write("o2", "o2\n");
    s.repo.commit("o2").unwrap();
    s.repo.checkout("main").unwrap();

    s.repo.checkout("feat/other").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "o1|o2",
        "D18 a copy of main's record, no fold: over-show"
    );
    assert_eq!(
        s.head_notice().as_deref(),
        Some(
            "switched main → feat/other: first time here, seen state carried from main; 2 files pending"
        )
    );
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "", "D18 accept-all on feat/other");
    s.repo.checkout("main").unwrap();
    assert_pile!(s.engine, s.root, "", "D18 main");
    let _ = s.engine.inspect_head(&s.root).unwrap();
    s.repo.checkout("feat/other").unwrap();
    assert_pile!(s.engine, s.root, "", "D18 the return: the parked record");
    assert_eq!(
        s.head_notice().as_deref(),
        Some("switched main → feat/other: 0 files differ from seen state"),
        "the second arrival is the return form"
    );
    s.restart();
    assert_pile!(s.engine, s.root, "", "D18 restart on feat/other");
}

#[test]
fn scenario_d19_detached_head_and_in_progress_keep_the_record() {
    // D18 after accept-all on feat/other.
    let mut s = Fresh::new("d19");
    assert_pile!(s.engine, s.root, "");
    s.repo.checkout_b("feat/other").unwrap();
    s.repo.write("o1", "o1\n");
    s.repo.commit("o1").unwrap();
    s.repo.write("o2", "o2\n");
    s.repo.commit("o2").unwrap();
    s.repo.checkout("main").unwrap();
    s.repo.checkout("feat/other").unwrap();
    assert_pile!(s.engine, s.root, "o1|o2");
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "");
    let _ = s.engine.inspect_head(&s.root).unwrap();

    s.repo
        .git(&["checkout", "-q", "--detach", "HEAD~1"])
        .unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "o2",
        "D19 the detach removes o2 from the worktree: over-show against the record"
    );
    assert_eq!(
        seen_branch(&s).as_deref(),
        Some("feat/other"),
        "a detached HEAD never switches"
    );
    let notice = s.head_notice().expect("the detach is a checkout");
    assert!(
        notice.starts_with("switched feat/other → ")
            && notice.ends_with(": 1 files differ from seen state"),
        "{notice}"
    );
    // An accept at the detached HEAD lands in the record in force (feat/other's).
    s.repo.write("o1", "o1 edited\n");
    assert!(s.accept_file("o1").ok());
    assert!(s.ledger().overrides.contains_key("o1"));
    assert_eq!(seen_branch(&s).as_deref(), Some("feat/other"));
    s.repo.checkout("feat/other").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D19 back on the branch: no switch, and the accept is there"
    );

    // A rebase stopped on a conflict: the record in force does not move.
    s.repo.commit("o1 edited").unwrap();
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
        "the rebase stops on f2"
    );
    let rg = s.engine.root(&s.root).unwrap().repo.as_ref().unwrap();
    assert_eq!(
        headstate::inspect(rg).unwrap().in_progress,
        Some(InProgress::Rebase)
    );
    let pile = s.scan();
    assert!(
        pile.row(b"f2").expect("f2 pending mid-rebase").conflicted,
        "conflict markers pending"
    );
    assert_eq!(
        seen_branch(&s).as_deref(),
        Some("feat/other"),
        "mid-rebase the record in force is unchanged"
    );
    s.repo.write("f2", "resolved\n");
    s.repo.git(&["add", "f2"]).unwrap();
    s.repo
        .git(&["-c", "core.editor=true", "rebase", "--continue"])
        .unwrap();
    let pile = s.scan();
    assert!(
        pile.row(b"o1").is_none() && pile.row(b"o2").is_none(),
        "B5's rule: the rebased commits re-flag nothing ({})",
        common::pile_string(&pile)
    );
    assert_eq!(seen_branch(&s).as_deref(), Some("feat/other"));

    // Detached, then a new branch: the first sight copies the record in force.
    s.repo.git(&["checkout", "-q", "--detach"]).unwrap();
    let _ = s.scan();
    s.repo.checkout_b("feat/from-detached").unwrap();
    let _ = s.scan();
    assert_eq!(seen_branch(&s).as_deref(), Some("feat/from-detached"));
    assert!(
        parked(&s).contains(&"feat/other".to_string()),
        "A was the record in force, feat/other: {:?}",
        parked(&s)
    );
}

#[test]
fn scenario_d20_deleted_recreated_renamed() {
    let mut s = d15_three_records("d20");
    s.repo.git(&["branch", "-D", "run-1"]).unwrap();
    s.repo.checkout("run-2").unwrap();
    assert_pile!(s.engine, s.root, "c.rs", "D20 any switch prunes");
    assert_eq!(
        parked(&s),
        vec!["main".to_string()],
        "the deleted branch's record is pruned; main is parked while run-2 is in force"
    );
    s.repo.checkout("main").unwrap();
    assert_pile!(s.engine, s.root, "");
    s.repo.checkout_b("run-1").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D20 the recreated run-1 is a first sight; nothing of the old record survives"
    );

    // A rename of the branch in force: re-label, no park, no first sight.
    s.repo.checkout("run-2").unwrap();
    assert_pile!(s.engine, s.root, "c.rs");
    let tree = s.ledger().seen_tree.clone();
    s.repo.git(&["branch", "-m", "run-2", "run-two"]).unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "c.rs",
        "D20 the rename leaves the pile unchanged"
    );
    assert_eq!(seen_branch(&s).as_deref(), Some("run-two"));
    assert_eq!(s.ledger().seen_tree, tree, "the same record, re-labelled");
    let names = parked(&s);
    assert!(
        !names.contains(&"run-2".to_string()) && !names.contains(&"run-two".to_string()),
        "no park and no first sight for a rename: {names:?}"
    );

    // A parked branch renamed: pruned at the next switch; the new name first-sights.
    s.repo.git(&["branch", "-m", "main", "trunk"]).unwrap();
    s.repo.checkout("trunk").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D20 trunk first-sights from the record in force"
    );
    assert_eq!(seen_branch(&s).as_deref(), Some("trunk"));
    assert_eq!(
        parked(&s),
        vec!["run-1".to_string(), "run-two".to_string()],
        "main's parked record is pruned with its ref"
    );
}

#[test]
fn scenario_d21_restart_and_offline_switches() {
    let mut s = d15_three_records("d21");
    s.repo.checkout("run-1").unwrap(); // lastcall is not watching
    s.restart();
    assert_pile!(
        s.engine,
        s.root,
        "a.rs|b.rs",
        "D21 the switch happens at open"
    );
    assert_eq!(seen_branch(&s).as_deref(), Some("run-1"));
    assert!(
        s.engine.inspect_head(&s.root).unwrap().is_none(),
        "no notice: nothing moved while lastcall watched"
    );
    s.repo.checkout("main").unwrap();
    s.restart();
    assert_pile!(s.engine, s.root, "", "D21 back on main at open");
    assert_eq!(seen_branch(&s).as_deref(), Some("main"));
}

/// Four records, the last of them first-sighted while lastcall was not running: `main`
/// seen first, `feat` with one commit of its own, back on `main`, then an offline
/// `checkout -b new` whose switch lands at open. Nothing describes that switch, so R8's
/// first-sight wording must not be waiting for whatever the user does next (verifier F3).
fn d21_first_sight_at_open(name: &str) -> Fresh {
    let mut s = Fresh::new(name);
    assert_pile!(s.engine, s.root, "", "first sight on main");
    s.repo.checkout_b("feat").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "feat starts as a copy of main's record"
    );
    s.repo.write("n.rs", "n\n");
    s.repo.commit("on feat").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "n.rs",
        "feat's own commit is pending there"
    );
    s.repo.checkout("main").unwrap();
    assert_pile!(s.engine, s.root, "", "main's parked record is back");
    // Not watching: the branch is cut and the switch is only seen at the next open.
    s.repo.checkout_b("new").unwrap();
    s.restart();
    assert_eq!(seen_branch(&s).as_deref(), Some("new"));
    s
}

#[test]
fn scenario_d21_a_first_sight_at_open_does_not_relabel_the_next_checkout() {
    let mut s = d21_first_sight_at_open("d21c");
    s.repo.checkout("feat").unwrap();
    assert_eq!(
        s.head_notice().as_deref(),
        Some("switched new → feat: 1 files differ from seen state"),
        "a return to a parked record keeps today's text"
    );
    assert_eq!(seen_branch(&s).as_deref(), Some("feat"));
}

#[test]
fn scenario_d21_a_first_sight_at_open_does_not_relabel_a_detach() {
    let mut s = d21_first_sight_at_open("d21d");
    let feat_tip = s.repo.git(&["rev-parse", "refs/heads/feat"]).unwrap();
    let short: String = feat_tip.trim().chars().take(7).collect();
    s.repo.git(&["checkout", "-q", "--detach", "feat"]).unwrap();
    assert_eq!(
        s.head_notice().as_deref(),
        Some(format!("switched new → {short}: 1 files differ from seen state").as_str()),
        "a detach performs no switch, so the plain checkout text"
    );
    assert_eq!(
        seen_branch(&s).as_deref(),
        Some("new"),
        "R4: the record in force stays while HEAD is detached"
    );
}

#[test]
fn scenario_d21_a_1_1_ledger_adopts_the_branch_without_a_fold() {
    let mut s = Fresh::new("d21b");
    s.repo.write("x.txt", "x\n");
    assert_pile!(s.engine, s.root, "x.txt");
    assert!(s.accept_file("x.txt").ok());
    s.repo.write("x.txt", "x edited\n");
    assert_pile!(
        s.engine,
        s.root,
        "x.txt",
        "D21 a pending file and an override"
    );
    s.repo.checkout_b("feat/y").unwrap();

    // The file a 1.1 binary would have left: no seen_branch, no branches.
    let path = ledger_file(&s);
    let text = std::fs::read_to_string(&path).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let obj = v.as_object_mut().unwrap();
    obj.insert(
        "schema_version".to_string(),
        serde_json::Value::String("1.1".to_string()),
    );
    obj.remove("seen_branch");
    obj.remove("branches");
    let text = format!("{}\n", serde_json::to_string_pretty(&v).unwrap());
    std::fs::write(&path, &text).unwrap();

    s.restart();
    assert_pile!(
        s.engine,
        s.root,
        "x.txt",
        "D21 the 1.1 file adopts HEAD's branch with no fold and no change to the pile"
    );
    assert_eq!(seen_branch(&s).as_deref(), Some("feat/y"));
    assert!(parked(&s).is_empty());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        text,
        "byte-identical until the next write"
    );
    assert!(s.accept_file("x.txt").ok());
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(after["schema_version"].as_str(), Some("1.2"));
    assert_eq!(after["seen_branch"].as_str(), Some("feat/y"));
}

#[test]
fn scenario_d22_two_processes_over_one_root() {
    let mut s = Fresh::new("d22");
    let env = s.repo.engine_env(s.state.path());
    let mut e2 = open_engine_with(
        s.repo.path(),
        &env,
        s.state.path(),
        Config::default(),
        EngineOptions::default(),
    );
    s.repo.checkout_b("feat/z").unwrap();
    s.repo.write("z1", "z1\n");
    s.repo.commit("z1 on feat/z").unwrap();
    assert_pile!(s.engine, s.root, "z1", "D22 engine 1 performs the switch");
    assert_pile!(
        e2,
        s.root,
        "z1",
        "D22 engine 2 adopts the file, no second first sight"
    );
    let row = e2.scan(&s.root).unwrap().row(b"z1").unwrap().clone();
    let rendered = Rendered::of(&row);

    s.repo.checkout("main").unwrap();
    assert_pile!(s.engine, s.root, "", "D22 engine 1 switches back to main");
    let out = e2
        .ops(&s.root)
        .unwrap()
        .accept_file(&rendered, &NoFault)
        .expect("accept_file");
    assert!(!out.ok(), "the accept is refused");
    assert_eq!(
        out.refused[0].to_string(),
        "branch changed under this accept (now main); try again"
    );
    assert!(!out.written, "nothing written");
    assert_pile!(e2, s.root, "", "D22 engine 2's next scan shows main's pile");
}

/// R5 covers the snapshot as well as the staged op (verifier F4). Once engine 2 has
/// adopted the branch engine 1 switched to, every `Ops` it builds is staged under the new
/// branch and the staged-op refusal has nothing left to compare; the pile engine 2 is still
/// holding was computed against the record it left, and a fold of it would write every one
/// of those rows, restamp `seen_at` and push an undo entry into the wrong record. The pile
/// carries the branch it was scanned under, so the accept is refused instead.
#[test]
fn scenario_d22_a_snapshot_rendered_on_the_branch_left_is_refused() {
    let mut s = Fresh::new("d22b");
    let env = s.repo.engine_env(s.state.path());
    let mut e2 = open_engine_with(
        s.repo.path(),
        &env,
        s.state.path(),
        Config::default(),
        EngineOptions::default(),
    );
    s.repo.write("f1", "edited on main\n");
    assert_pile!(s.engine, s.root, "f1", "f1 pending on main");
    let snapshot = e2.scan(&s.root).unwrap();
    assert_eq!(
        snapshot.seen_branch.as_deref(),
        Some("main"),
        "the pile says which record it was computed against"
    );
    assert_eq!(common::pile_string(&snapshot), "f1");
    let rendered = Rendered::of(snapshot.row(b"f1").unwrap());

    // Engine 1 performs the switch; the uncommitted f1 travels with the checkout.
    s.repo.checkout_b("feat").unwrap();
    assert_pile!(s.engine, s.root, "f1", "engine 1 first-sights feat");

    // Engine 2 still believes it is on main: its restore is refused, and its rescan adopts.
    let restored = e2
        .restore(&s.root, RestoreRequest::File(rendered))
        .expect("restore");
    assert!(!restored.outcome.ok(), "{:?}", restored.outcome);
    assert_eq!(
        restored.outcome.refused[0].message("restored"),
        "branch changed under this restore (now feat); try again"
    );
    let adopted = e2.scan(&s.root).unwrap();
    assert_eq!(adopted.seen_branch.as_deref(), Some("feat"));

    let before = e2.root(&s.root).unwrap().ledger.clone();
    let out = e2
        .ops(&s.root)
        .unwrap()
        .accept_all(&snapshot, &NoFault)
        .expect("accept_all");
    assert!(!out.ok(), "the stale snapshot is refused: {out:?}");
    assert!(!out.written, "nothing written");
    assert_eq!(
        out.refused[0].to_string(),
        "branch changed under this accept (now feat); try again"
    );
    let after = e2.root(&s.root).unwrap().ledger.clone();
    assert_eq!(
        after.seen_tree, before.seen_tree,
        "feat's tree is untouched"
    );
    assert!(
        after.overrides.is_empty() && after.undo.is_empty(),
        "no override and no undo entry landed in feat's record: {:?}",
        after.overrides.keys().collect::<Vec<_>>()
    );
    assert_pile!(e2, s.root, "f1", "the row is still pending on feat");
    // The pile engine 2 scans now is feat's, and accepting *that* one lands normally.
    let fresh = e2.scan(&s.root).unwrap();
    let out = e2
        .ops(&s.root)
        .unwrap()
        .accept_all(&fresh, &NoFault)
        .expect("accept_all");
    assert!(out.ok() && out.written, "{out:?}");
    assert_pile!(e2, s.root, "", "accepted into feat's own record");
}

#[test]
fn scenario_d23_uncommitted_work_and_its_accepted_hunk_survive_checkout_b() {
    let mut s = Fresh::new("d23");
    s.repo
        .write("f1", "A1\na2\na3\na4\na5\na6\na7\na8\na9\nA10\n");
    let pile = assert_pile!(s.engine, s.root, "f1");
    assert_eq!(pile.row(b"f1").unwrap().hunks.len(), 2, "two hunks");
    assert!(s.accept_hunk("f1", 0).ok());
    let pile = assert_pile!(s.engine, s.root, "f1", "D23 hunk 2 only");
    assert_eq!(pile.row(b"f1").unwrap().hunks.len(), 1);
    let over = s.ledger().overrides["f1"].clone();

    s.repo.checkout_b("feat/w").unwrap();
    let pile = assert_pile!(s.engine, s.root, "f1", "D23 the copy carries the override");
    assert_eq!(pile.row(b"f1").unwrap().hunks.len(), 1, "exactly hunk 2");
    assert_eq!(s.ledger().overrides["f1"], over);
    // R8 is explicit that a first sight **at the same commit** keeps today's wording, and
    // names D23 among the scenarios that keep their strings; the scenario text, written
    // before that rule, shows the first-sight form for this same shape. The rule wins: no
    // discriminator distinguishes this `checkout -b` from B2's, which is frozen as
    // `(same commit)`. Recorded for a ruling in the phase report.
    assert_eq!(
        s.head_notice().as_deref(),
        Some("switched main → feat/w (same commit)")
    );
    s.repo.checkout("main").unwrap();
    let pile = assert_pile!(s.engine, s.root, "f1", "D23 back on main");
    assert_eq!(pile.row(b"f1").unwrap().hunks.len(), 1);
    assert_eq!(s.ledger().overrides["f1"], over);
    // The same commit in the other direction, and `main`'s own record is loaded back.
    assert_eq!(
        s.head_notice().as_deref(),
        Some("switched feat/w → main (same commit)")
    );
}

/// D24's fixture: first sight on `main` at `c1` (`P = f1` at `v1` and seen, `Q = q` absent,
/// `f = f2` at `v1`); the agent commits `c2` (`P = v2`, `Q` added, `f = v2`) and `c3` (`P`
/// and `Q` deleted); the user accepts `f` only. Returns the fixture and `c2`'s hash.
fn d24_main_at_c3(name: &str) -> (Fresh, String) {
    let mut s = Fresh::new(name);
    assert_pile!(s.engine, s.root, "", "D24 first sight on main at c1");
    s.repo.write("f1", "P v2\n");
    s.repo.write("q", "Q w1\n");
    s.repo.write("f2", "f v2\n");
    let c2 = s.repo.commit("c2").unwrap();
    s.repo.remove("f1");
    s.repo.remove("q");
    s.repo.commit("c3").unwrap();
    let pile = assert_pile!(s.engine, s.root, "f1|f2", "D24 P's deletion and f pending");
    assert_eq!(pile.row(b"f1").unwrap().change, Change::Deleted);
    assert!(
        pile.row(b"q").is_none(),
        "Q is absent against an absent baseline"
    );
    assert!(s.accept_file("f2").ok());
    assert_pile!(
        s.engine,
        s.root,
        "f1",
        "D24 on main at c3: only P's deletion"
    );
    let _ = s.engine.inspect_head(&s.root).unwrap();
    (s, c2)
}

/// D24: the fold takes only what the record has accepted at the departed tip, and never a
/// blob for a path it holds as absent (verifier F2).
#[test]
fn scenario_d24_the_fold_takes_only_what_the_record_saw_at_the_departed_tip() {
    let (mut s, c2) = d24_main_at_c3("d24");
    s.repo.git(&["branch", "mid", &c2]).unwrap();
    s.repo.checkout("mid").unwrap();
    let pile = assert_pile!(
        s.engine,
        s.root,
        "f1|q",
        "D24 neither P nor Q is folded onto mid"
    );
    assert_eq!(
        pile.row(b"f1").unwrap().change,
        Change::Modified,
        "P's baseline v1 is not main's tip (absent), so it over-shows as v1 → v2"
    );
    assert_eq!(
        pile.row(b"q").unwrap().change,
        Change::Added,
        "Q's baseline is absent and mid has a blob: never folded, shown as added"
    );
    assert!(
        pile.row(b"f2").is_none(),
        "f does not differ between the tips and is not considered"
    );
    assert_eq!(
        s.head_notice().as_deref(),
        Some("switched main → mid: first time here, seen state carried from main; 2 files pending")
    );
    assert_eq!(seen_branch(&s).as_deref(), Some("mid"));
    s.repo.checkout("main").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "f1",
        "D24 main's own record comes back unchanged"
    );
}

/// D24 Variant A: an accepted deletion is a baseline of its own, and the fold must not
/// refill it with the blob the arrived-on tip happens to carry.
#[test]
fn scenario_d24_variant_a_an_accepted_deletion_is_never_refilled_by_the_fold() {
    let (mut s, c2) = d24_main_at_c3("d24a");
    assert!(s.accept_file("f1").ok(), "P's deletion is accepted on main");
    assert_pile!(s.engine, s.root, "", "D24A nothing pending on main");
    let _ = s.engine.inspect_head(&s.root).unwrap();
    s.repo.git(&["branch", "mid", &c2]).unwrap();
    s.repo.checkout("mid").unwrap();
    let pile = assert_pile!(s.engine, s.root, "f1|q", "D24A P is shown, not baselined");
    assert_eq!(
        pile.row(b"f1").unwrap().change,
        Change::Added,
        "P's baseline is absent and mid has v2, so it shows as added"
    );
    assert_eq!(pile.row(b"q").unwrap().change, Change::Added);
}

/// D24 Variant B, the positive fold: a path the record accepted **at** the departed tip
/// still folds to the arrived-on tip's content, override and all.
#[test]
fn scenario_d24_variant_b_content_accepted_at_the_departed_tip_still_folds() {
    // The repo is on `main` at `c1` and `future` is cut from it before lastcall opens, so
    // the return to `main` is a genuine first sight (the D14 shape). Opening on `main`
    // first would park its record and the return would load it back, with no fold to test.
    let repo = FixtureRepo::new("d24b").unwrap();
    repo.checkout_b("future").unwrap();
    let mut s = Fresh::over(repo, Config::default(), EngineOptions::default(), false);
    assert_eq!(seen_branch(&s).as_deref(), Some("future"));
    assert!(parked(&s).is_empty(), "D24B no record for main");
    s.repo.write("f2", "f v2\n");
    s.repo.commit("f = v2 on future").unwrap();
    assert_pile!(s.engine, s.root, "f2", "D24B the agent's commit is pending");
    assert!(s.accept_file("f2").ok());
    assert_pile!(s.engine, s.root, "", "D24B accepted on future");
    assert!(s.ledger().overrides.contains_key("f2"));
    let _ = s.engine.inspect_head(&s.root).unwrap();

    s.repo.checkout("main").unwrap();
    assert_pile!(s.engine, s.root, "", "D24B the fold takes main's v1 for f");
    assert_eq!(
        s.head_notice().as_deref(),
        Some(
            "switched future → main: first time here, seen state carried from future; 0 files pending"
        )
    );
    assert!(
        s.ledger().overrides.is_empty(),
        "the folded path loses the blob and mode of its override"
    );
    s.repo.checkout("future").unwrap();
    assert_pile!(s.engine, s.root, "", "D24B future's own record comes back");
}

// ---------------------------------------------------------------------------------------
// D25: the fold's target is the merge-base of the two tips, not the branch it left.
// ---------------------------------------------------------------------------------------

/// D25: a branch cut from the shared ancestor and committed to before lastcall looks.
///
/// The whole sequence `checkout main && checkout -b feat2 && commit` runs with no scan
/// between the commands, so the record in force when the scan finally runs is `feat`'s: the
/// two tips have diverged and the fold takes the merge-base, which is the commit both
/// branches were cut from.
#[test]
fn scenario_d25_a_branch_cut_from_the_shared_ancestor_folds_to_the_merge_base() {
    let mut s = Fresh::new("d25");
    assert_pile!(s.engine, s.root, "", "D25 first sight on main at c1");
    s.repo.checkout_b("feat").unwrap();
    assert_pile!(s.engine, s.root, "", "D25 feat is a copy of main's record");
    for n in ["a.rs", "b.rs", "c.rs"] {
        s.repo.write(n, format!("{n}\n"));
    }
    s.repo.commit("c2 adds a, b and c").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "a.rs|b.rs|c.rs",
        "D25 the agent's commit is pending on feat"
    );
    for n in ["a.rs", "b.rs", "c.rs"] {
        assert!(s.accept_file(n).ok());
    }
    assert_pile!(s.engine, s.root, "", "D25 the three files accepted on feat");
    let _ = s.engine.inspect_head(&s.root).unwrap();

    // One shell line: lastcall observes no intermediate state between these three commands.
    s.repo.checkout("main").unwrap();
    s.repo.checkout_b("feat2").unwrap();
    s.repo.write("d.rs", "d\n");
    s.repo.commit("c3 adds d").unwrap();

    let pile = assert_pile!(
        s.engine,
        s.root,
        "d.rs",
        "D25 on feat2: the accepted work folds to the merge-base, only d shows"
    );
    assert_eq!(
        pile.row(b"d.rs").unwrap().change,
        Change::Added,
        "feat2's own commit is not in the fold's path list and shows"
    );
    // The notice reports the last thing that moved HEAD, and in this sequence that is the
    // commit, not the checkout: git's reflog has `commit: c3 adds d` on top, so the head
    // watcher classifies the move as a commit and never reaches R8's first-sight form. The
    // first sight itself happened all the same, which is what the pile above shows.
    assert_eq!(
        s.head_notice().as_deref(),
        Some("committed on feat2 (1 commit)")
    );
    assert_eq!(seen_branch(&s).as_deref(), Some("feat2"));
    assert!(
        s.ledger().overrides.is_empty(),
        "the folded paths lost their overrides"
    );
    s.restart();
    assert_pile!(s.engine, s.root, "d.rs", "D25 restart on feat2");

    assert!(s.accept_file("d.rs").ok());
    s.repo.checkout("feat").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D25 feat's parked record: its own work is still accepted"
    );
    s.repo.checkout("main").unwrap();
    assert_pile!(s.engine, s.root, "", "D25 main: nothing pending at c1");
}

/// D25 Variant A, timing independence: the same sequence with a scan after every git
/// command reaches the same pile at every step the two runs share.
#[test]
fn scenario_d25_variant_a_a_scan_after_every_command_reaches_the_same_pile() {
    let mut s = Fresh::new("d25a");
    assert_pile!(s.engine, s.root, "", "D25A first sight on main at c1");
    s.repo.checkout_b("feat").unwrap();
    assert_pile!(s.engine, s.root, "", "D25A feat is a copy of main's record");
    for n in ["a.rs", "b.rs", "c.rs"] {
        s.repo.write(n, format!("{n}\n"));
    }
    s.repo.commit("c2 adds a, b and c").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "a.rs|b.rs|c.rs",
        "D25A the agent's commit is pending on feat"
    );
    for n in ["a.rs", "b.rs", "c.rs"] {
        assert!(s.accept_file(n).ok());
    }
    assert_pile!(s.engine, s.root, "", "D25A accepted on feat");
    let _ = s.engine.inspect_head(&s.root).unwrap();
    s.repo.checkout("main").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D25A main's parked record, no deletions"
    );
    s.repo.checkout_b("feat2").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D25A feat2 is a copy of main's record"
    );
    s.repo.write("d.rs", "d\n");
    s.repo.commit("c3 adds d").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "d.rs",
        "D25A the same pile as the unobserved run"
    );
    assert!(s.accept_file("d.rs").ok());
    s.repo.checkout("feat").unwrap();
    assert_pile!(s.engine, s.root, "", "D25A feat's parked record");
    s.repo.checkout("main").unwrap();
    assert_pile!(s.engine, s.root, "", "D25A main");
}

/// D25 Variant B, the hide the ancestor form admitted: the cherry-picked copy of content
/// accepted on a run branch must show again on the feature branch (D16's rule), and the
/// carried override must not hide it.
#[test]
fn scenario_d25_variant_b_a_cherry_pick_is_never_hidden_by_the_carried_override() {
    let mut s = d15_three_records("d25b");
    s.repo.checkout("run-1").unwrap();
    assert_pile!(s.engine, s.root, "a.rs|b.rs", "D25B run-1's work");
    assert!(s.accept_all().ok());
    s.repo.checkout("run-2").unwrap();
    assert_pile!(s.engine, s.root, "c.rs", "D25B run-2's work");
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "", "D25B accepted on run-2");

    // lastcall is closed while the run's commit is cherry-picked onto a branch cut from
    // main: a new commit id, so run-2 is not an ancestor of feat/x.
    s.repo
        .git(&["checkout", "-q", "-b", "feat/x", "main"])
        .unwrap();
    s.repo.git(&["cherry-pick", "main..run-2"]).unwrap();
    s.restart();

    assert_pile!(
        s.engine,
        s.root,
        "c.rs",
        "D25B the cherry-picked content shows once more, by design (D16)"
    );
    assert_eq!(seen_branch(&s).as_deref(), Some("feat/x"));
    assert!(
        s.engine.inspect_head(&s.root).unwrap().is_none(),
        "no notice: nothing moved while lastcall watched"
    );
    assert!(
        s.ledger().overrides.is_empty(),
        "c.rs was accepted at run-2's tip, so the fold took the merge-base's absence"
    );
}

// ---------------------------------------------------------------------------------------
// D26 — the fold takes only entries that are already seen state.
// ---------------------------------------------------------------------------------------

/// One git plumbing command against `repo`'s git dir with an index file of its own and an
/// optional stdin, in the same isolated environment the fixture uses.
///
/// [`FixtureRepo::git`] removes `GIT_INDEX_FILE` and offers no stdin, so a branch built
/// without a checkout (D26 Variant B) needs its own runner.
fn plumb(
    cwd: &std::path::Path,
    index: &std::path::Path,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_INDEX_FILE", index)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
        .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
        .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
        .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL)
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    if let Some(bytes) = stdin {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(bytes)
            .expect("write stdin");
    }
    let out = child.wait_with_output().expect("git output");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A branch built from `base` by plumbing alone, with `sets` written over `base`'s tree.
/// It is never checked out, so it never becomes the branch in force and never gets a
/// record of its own.
fn plumb_branch(repo: &FixtureRepo, name: &str, base: &str, sets: &[(&str, &str)]) -> String {
    let index = repo.parent_dir().join(format!("{name}.index"));
    let _ = std::fs::remove_file(&index);
    let cwd = repo.path();
    plumb(cwd, &index, &["read-tree", base], None);
    for (path, contents) in sets {
        let blob = plumb(
            cwd,
            &index,
            &["hash-object", "-w", "--stdin"],
            Some(contents.as_bytes()),
        );
        plumb(
            cwd,
            &index,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},{path}"),
            ],
            None,
        );
    }
    let tree = plumb(cwd, &index, &["write-tree"], None);
    let commit = plumb(
        cwd,
        &index,
        &[
            "commit-tree",
            &tree,
            "-p",
            base,
            "-m",
            &format!("{name} built without a checkout"),
        ],
        None,
    );
    plumb(
        cwd,
        &index,
        &["update-ref", &format!("refs/heads/{name}"), &commit],
        None,
    );
    let _ = std::fs::remove_file(&index);
    commit
}

/// D26, the main case: a version committed and put back before it was ever accepted is
/// not seen state, so a branch cut at that commit must show it.
///
/// `main` goes c1 (first sight) → c2 (f1 = v2, pending, never accepted) → c3 (f1 back to
/// the first-sight content). `B` is cut at c2, so the merge-base of the two tips is c2
/// itself and its entry for f1 is the version that was never on a screen.
#[test]
fn scenario_d26_a_version_that_was_never_accepted_is_not_folded_in() {
    let mut s = Fresh::new("d26");
    assert_pile!(s.engine, s.root, "", "D26 first sight on main at c1");
    let c1 = s.head();
    let seed = SEED_FILES[0].1;

    s.repo.write("f1", "P v2\n");
    s.repo.commit("c2 sets f1 to v2").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "f1",
        "D26 c2 is pending and never accepted"
    );
    let c2 = s.head();

    s.repo.write("f1", seed);
    s.repo.commit("c3 puts f1 back").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D26 c3 restores the entry lastcall saw"
    );

    s.repo.git(&["checkout", "-q", "-b", "B", &c2]).unwrap();
    let pile = assert_pile!(s.engine, s.root, "f1", "D26 v2 was never seen state");
    let row = pile.row(b"f1").unwrap();
    assert_eq!(row.change, Change::Modified);
    assert_eq!(
        row.baseline.as_ref().unwrap().oid.to_string(),
        s.repo
            .git(&["rev-parse", &format!("{c1}:f1")])
            .unwrap()
            .trim(),
        "the record still holds the first-sight entry"
    );

    assert!(s.accept_file("f1").ok());
    assert_pile!(s.engine, s.root, "", "D26 v2 accepted on B");
    s.repo.checkout("main").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D26 main's parked record is untouched"
    );
}

/// D26 Variant A, the same shape with a mode flip: the executable bit set at c2 and
/// cleared at c3 was never accepted, so `B` at c2 must show the mode.
#[test]
fn scenario_d26_variant_a_a_mode_that_was_never_accepted_is_not_folded_in() {
    let mut s = Fresh::new("d26a");
    assert_pile!(s.engine, s.root, "", "D26A first sight on main at c1");

    s.repo.chmod_x("f2", true);
    s.repo.commit("c2 makes f2 executable").unwrap();
    assert_pile!(s.engine, s.root, "f2", "D26A the bit is pending on main");
    let c2 = s.head();

    s.repo.chmod_x("f2", false);
    s.repo.commit("c3 clears the bit").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D26A c3 restores the mode lastcall saw"
    );

    s.repo.git(&["checkout", "-q", "-b", "B", &c2]).unwrap();
    let pile = assert_pile!(s.engine, s.root, "f2", "D26A the bit was never seen state");
    assert_eq!(pile.row(b"f2").unwrap().change, Change::Mode);
    assert!(
        s.ledger().overrides.is_empty(),
        "the fold wrote no override"
    );
}

/// D26 Variant B: a branch built without a checkout never has a record, so nothing in the
/// ledger has ever shown its content. Merged into the watched branch with `-X ours` and
/// then checked out, its own tip is the merge-base, and the entry it carries for f1 must
/// still show.
#[test]
fn scenario_d26_variant_b_a_never_visited_branch_is_not_seen_state() {
    let mut s = Fresh::new("d26b");
    assert_pile!(s.engine, s.root, "", "D26B first sight on main at c1");
    let c1 = s.head();

    s.repo.checkout_b("A").unwrap();
    assert_pile!(s.engine, s.root, "", "D26B A is a copy of main's record");
    s.repo.write("f1", "P v2\n");
    s.repo.commit("c2 sets f1 to v2 on A").unwrap();
    assert_pile!(s.engine, s.root, "f1", "D26B the commit on A is pending");
    assert!(s.accept_file("f1").ok());
    assert_pile!(s.engine, s.root, "", "D26B v2 accepted on A");

    plumb_branch(&s.repo, "other", &c1, &[("f1", "P vm\n"), ("k2", "k2\n")]);
    s.repo
        .git(&[
            "merge",
            "-q",
            "--no-ff",
            "-X",
            "ours",
            "-m",
            "merge other into A",
            "other",
        ])
        .unwrap();
    assert_pile!(s.engine, s.root, "k2", "D26B the merge brings k2 in");
    assert!(s.accept_file("k2").ok());
    assert_pile!(s.engine, s.root, "", "D26B k2 accepted on A");

    s.repo.checkout("other").unwrap();
    let pile = assert_pile!(
        s.engine,
        s.root,
        "f1",
        "D26B other's f1 was never seen state"
    );
    assert_eq!(pile.row(b"f1").unwrap().change, Change::Modified);
}

/// D26 Variant C, the seed folds back: a deletion accepted on `b1` must not survive the
/// walk back to `b0`, whose entry at the merge-base is the one lastcall first saw. This
/// is the over-show the seen-state target removes.
#[test]
fn scenario_d26_variant_c_the_first_sight_entry_folds_back() {
    let mut s = Fresh::new("d26c");
    assert_pile!(s.engine, s.root, "", "D26C first sight on main at c1");

    // Two cuts with no scan between them, so b0 never gets a record of its own.
    s.repo.git(&["checkout", "-q", "-b", "b0"]).unwrap();
    s.repo.git(&["checkout", "-q", "-b", "b1"]).unwrap();
    s.repo.remove("f1");
    s.repo.commit("c2 removes f1 on b1").unwrap();
    assert_pile!(s.engine, s.root, "f1", "D26C the removal is pending on b1");
    assert!(s.accept_file("f1").ok());
    assert_pile!(s.engine, s.root, "", "D26C the removal accepted on b1");

    s.repo.checkout("b0").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "D26C the first-sight entry folds back on b0"
    );
    assert!(
        s.ledger().overrides.is_empty(),
        "the accepted deletion is spent by the fold"
    );
}
