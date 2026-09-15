//! Scenario suite D — file-system and repo-shape edge cases (docs/spec/01-scenarios.md §D).

mod common;

use common::Fresh;
use lastcall_engine::config::Config;
use lastcall_engine::engine::EngineOptions;
use lastcall_engine::git::Mode;
use lastcall_engine::ops::Refused;
use lastcall_engine::roots::Badge;
use lastcall_engine::scan::{Change, Collapsed, Rename, probe_case_insensitive};
use lastcall_testkit::assert_pile;
use lastcall_testkit::engine::open_engine;
use lastcall_testkit::fixture_repo::FixtureRepo;
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
