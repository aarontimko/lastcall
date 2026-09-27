//! Scenario suite A — core seen-state loop (docs/spec/01-scenarios.md §A). Expected pile
//! strings are copied from `scripts/harness/scenarios.sh` for the (H) scenarios.

mod common;

use common::Fresh;
use lastcall_engine::engine::RestoreRequest;
use lastcall_engine::git::Oid;
use lastcall_engine::ledger::FlagHunk;
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
fn scenario_a7_restore_deletion_recreates_the_baseline_blob() {
    let mut s = Fresh::new("a7-restore");
    let baseline_bytes = s.bytes_at("f3");
    s.repo.remove("f3");
    let pile = assert_pile!(s.engine, s.root, "f3", "A7 deletion pending");
    let row = pile.row(b"f3").unwrap();
    assert_eq!(row.change, Change::Deleted);
    let baseline_oid = row
        .baseline
        .as_ref()
        .expect("a deletion has a baseline")
        .oid
        .clone();

    let out = s.restore_file("f3");
    assert!(out.outcome.ok(), "{:?}", out.outcome);
    assert!(
        !out.outcome.written,
        "a restore never writes the ledger: the baseline stays where it was"
    );
    // Byte-identical to the baseline blob, and hash-compared: the file that comes back is
    // the object the store still holds, not a re-render of it.
    assert_eq!(s.bytes_at("f3"), baseline_bytes);
    assert_eq!(
        s.store().hash_bytes(&s.bytes_at("f3")).unwrap(),
        baseline_oid,
        "the recreated file hashes to the baseline blob"
    );
    assert_pile!(s.engine, s.root, "", "A7 restore clears the deletion row");
    assert!(
        !s.ledger().overrides.contains_key("f3"),
        "no override was created by the restore"
    );
}

/// Gate item 1, engine half: the mid-review mutation. An agent writes to the file between
/// the frame the human is looking at and the key they press. The restore must refuse and
/// leave the agent's bytes alone — the CAS is the whole guard, and this is the case it
/// exists for.
#[test]
fn scenario_gate1_mid_review_mutation_refuses_the_restore_and_keeps_the_mutated_bytes() {
    let mut s = Fresh::new("gate1");
    s.repo
        .write("f1", "A1\na2\na3\na4\na5\na6\na7\na8\na9\nA10\n");
    // The frame the human is reading.
    let row = s.row("f1");
    assert_eq!(row.hunks.len(), 2);
    let rendered = Rendered::of(&row);

    // The agent writes again while they read it.
    let mutated = b"A1\na2\na3\na4\na5\na6\na7\na8\na9\nA10\nAGENT WROTE THIS\n";
    s.repo.write("f1", mutated);

    let restored = s
        .engine
        .restore(
            &s.root,
            RestoreRequest::Hunk {
                rendered,
                hunks: row.hunks.clone(),
                index: 0,
            },
        )
        .expect("restore");
    assert!(
        matches!(&restored.outcome.refused[..], [Refused::Moved { path, .. }] if path == b"f1"),
        "{:?}",
        restored.outcome
    );
    assert_eq!(
        s.bytes_at("f1"),
        mutated,
        "byte compare: the agent's write survived untouched"
    );
    assert!(!restored.outcome.written, "no ledger write");
    // The pile that comes back is the rescan, so the human sees the new delta rather than
    // the frame they acted on.
    let after = restored.pile.row(b"f1").expect("f1 is still pending");
    assert_eq!(after.hunks.len(), 2);
    assert!(
        after.hunks[1]
            .lines
            .iter()
            .any(|(_, l)| l == b"AGENT WROTE THIS\n"),
        "the returned pile shows the new delta: {:?}",
        after.hunks
    );
    assert_eq!(
        Refused::Moved {
            path: b"f1".to_vec(),
            live: None
        }
        .message("restored"),
        "f1: changed since rendered; not restored",
        "the wording the PTY half (deliverable 11) asserts on the status line"
    );
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
        .flag(b"f1", "why is this unwrap safe?", None, None, &NoFault)
        .unwrap();
    assert!(out.ok());
    let pile = assert_pile!(s.engine, s.root, "f1");
    let row = pile.row(b"f1").unwrap();
    assert_eq!(
        row.flags.first().map(|f| f.note.as_str()),
        Some("why is this unwrap safe?")
    );
    assert!(row.flags[0].hunk.is_none(), "a file flag carries no hunk");
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
        over.flags
            .iter()
            .map(|f| f.note.as_str())
            .collect::<Vec<_>>(),
        vec!["why is this unwrap safe?"]
    );
}

/// A8, extended (Amendment v1.7): two **hunk** flags on one file are both retained through
/// accept-all, oldest first, each with the hunk text as it was on screen.
#[test]
fn scenario_a8_two_hunk_flags_on_one_file_survive_accept_all() {
    let mut s = Fresh::new("a8-hunks");
    s.repo
        .write("f1", "A1\na2\na3\na4\na5\na6\na7\na8\na9\nA10\n");
    let pile = assert_pile!(s.engine, s.root, "f1");
    let row = pile.row(b"f1").unwrap();
    assert_eq!(row.hunks.len(), 2, "an edit at each end is two hunks");
    let capture = |h: &lastcall_engine::hunks::Hunk| FlagHunk {
        index: h.index,
        header: format!(
            "@@ -{},{} +{},{} @@",
            h.old_range.start + 1,
            h.old_range.len(),
            h.new_range.start + 1,
            h.new_range.len()
        ),
        text: String::from_utf8_lossy(
            &h.lines
                .iter()
                .flat_map(|(_, l)| l.clone())
                .collect::<Vec<u8>>(),
        )
        .into_owned(),
    };
    let (h0, h1) = (capture(&row.hunks[0]), capture(&row.hunks[1]));
    for (note, hunk) in [("first line?", h0.clone()), ("last line?", h1.clone())] {
        assert!(
            s.engine
                .ops(&s.root)
                .unwrap()
                .flag(b"f1", note, Some(hunk), None, &NoFault)
                .unwrap()
                .ok()
        );
    }
    let pile = assert_pile!(s.engine, s.root, "f1");
    let row = pile.row(b"f1").unwrap();
    assert_eq!(
        row.flags
            .iter()
            .map(|f| f.note.as_str())
            .collect::<Vec<_>>(),
        vec!["first line?", "last line?"],
        "oldest first"
    );
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "");
    let over = s.ledger().overrides.get("f1").expect("flags retained");
    assert!(over.blob.is_none(), "accept-all left a flag-only override");
    assert_eq!(over.flags.len(), 2);
    assert_eq!(over.flags[0].hunk.as_ref().unwrap(), &h0);
    assert_eq!(over.flags[1].hunk.as_ref().unwrap(), &h1);
    // The captured text is what was on screen, not a re-derivation from a moved file.
    assert!(over.flags[0].hunk.as_ref().unwrap().text.contains("A1"));
}

// ---------------------------------------------------------------------------------------
// A9 — `skip_globs` keeps repositories and watched-folder files out (Amendment v1.15,
// Phase 14 deliverable E).
// ---------------------------------------------------------------------------------------

mod a9 {
    use std::path::{Path, PathBuf};

    use lastcall_engine::config::{Config, DraftInitial};
    use lastcall_engine::engine::Engine;
    use lastcall_engine::roots::Badge;
    use lastcall_engine::status::StatusReport;
    use lastcall_engine::store::RootKind;
    use lastcall_testkit::engine::open_engine;
    use lastcall_testkit::fixture_repo::FixtureRepo;
    use lastcall_testkit::tmp::TempDir;

    /// `W/r`: a tracked `t.rs`, a gitignored `z_ignore/` watched with its whole tree,
    /// holding `notes.md`, `evals/run.log` and two clones `evals/c1`, `evals/c2`.
    struct A9 {
        repo: FixtureRepo,
        state: TempDir,
        w: PathBuf,
        r: PathBuf,
    }

    impl A9 {
        fn new() -> Self {
            let mut repo = FixtureRepo::new("r").unwrap();
            repo.commit_files(&[("t.rs", "t\n"), (".gitignore", "z_ignore/\n")], "t")
                .unwrap();
            repo.write("z_ignore/notes.md", "notes\n");
            repo.write("z_ignore/evals/run.log", "log\n");
            for clone in ["c1", "c2"] {
                Self::clone_into(&repo, &format!("z_ignore/evals/{clone}"));
            }
            let w = std::fs::canonicalize(repo.parent_dir()).unwrap();
            let r = std::fs::canonicalize(repo.path()).unwrap();
            Self {
                repo,
                state: TempDir::new("lc-a9-state"),
                w,
                r,
            }
        }

        fn clone_into(repo: &FixtureRepo, rel: &str) {
            repo.git(&["init", "-q", rel]).unwrap();
            let at = repo.path().join(rel);
            repo.write(&format!("{rel}/x"), "x\n");
            repo.git_at(&at, &["add", "x"]).unwrap();
            repo.git_at(&at, &["commit", "-qm", "x"]).unwrap();
        }

        fn config(skip: &[&str]) -> Config {
            Config {
                draft_dirs: vec!["z_ignore/**".to_owned()],
                draft_initial: DraftInitial::Pending,
                skip_globs: skip.iter().map(|s| (*s).to_owned()).collect(),
                ..Config::default()
            }
        }

        /// Open with `parent_dirs = [parent]`, launched from `W` so `r` is no launch
        /// directory, and let discovery take in what the first scans report.
        fn open(&self, parent: &Path, skip: &[&str]) -> Engine {
            let env = self.repo.engine_env(self.state.path()).with_cwd(&self.w);
            let mut engine = open_engine(parent, &env, self.state.path(), Self::config(skip));
            settle(&mut engine);
            engine
        }

        fn at(&self, rel: &str) -> PathBuf {
            self.r.join(rel)
        }
    }

    /// Scan until discovery stops changing (a nested report promotes on the next pass).
    fn settle(engine: &mut Engine) {
        for _ in 0..3 {
            engine.scan_all();
        }
    }

    fn rows(engine: &mut Engine, root: &Path) -> Vec<String> {
        let pile = engine.scan(root).unwrap();
        let mut out: Vec<String> = pile
            .rows
            .iter()
            .map(|r| String::from_utf8_lossy(&r.path).into_owned())
            .collect();
        out.sort();
        out
    }

    /// `status --json`'s roots, and each root's pending paths.
    fn status(engine: &mut Engine) -> Vec<(String, Vec<String>)> {
        let json = StatusReport::build(engine, None).unwrap().to_json();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        v["roots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|root| {
                let mut paths: Vec<String> = root["pending"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|row| row["path"].as_str().unwrap().to_owned())
                    .collect();
                paths.sort();
                (root["root"].as_str().unwrap().to_owned(), paths)
            })
            .collect()
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn scenario_a9_skip_globs_keep_clones_and_their_files_out() {
        let a = A9::new();
        let (c1, c2, draft) = (
            a.at("z_ignore/evals/c1"),
            a.at("z_ignore/evals/c2"),
            a.at("z_ignore"),
        );

        // Without the key: both clones are roots badged nested, and `run.log` is a row.
        let mut e = a.open(&a.w, &[]);
        assert_eq!(
            e.root_paths(),
            vec![a.r.clone(), draft.clone(), c1.clone(), c2.clone()]
        );
        for clone in [&c1, &c2] {
            let badge = e.root(clone).unwrap().badge.clone();
            assert!(
                matches!(badge, Some(Badge::NestedIn(_))),
                "{clone:?}: {badge:?}"
            );
        }
        assert_eq!(e.root(&draft).unwrap().kind, RootKind::Draft);
        assert_eq!(rows(&mut e, &draft), vec!["evals/run.log", "notes.md"]);
        let stores: Vec<PathBuf> = [&c1, &c2]
            .iter()
            .map(|c| e.root(c).unwrap().paths.repo_dir.clone())
            .collect();
        drop(e);

        // With the sponsor's line, at a restart.
        a.repo.write("t.rs", "t edited\n");
        let skip = ["*/z_ignore/**/evals/**"];
        let mut e = a.open(&a.w, &skip);
        assert_eq!(e.root_paths(), vec![a.r.clone(), draft.clone()]);
        assert_eq!(
            rows(&mut e, &draft),
            vec!["notes.md"],
            "run.log is not a row"
        );
        assert_eq!(
            rows(&mut e, &a.r),
            vec!["t.rs"],
            "the key never reaches a listed repository's own files"
        );
        assert_eq!(
            status(&mut e),
            vec![
                (s(&a.r), vec!["t.rs".to_owned()]),
                (s(&draft), vec!["notes.md".to_owned()]),
            ],
            "status --json agrees"
        );
        for store in &stores {
            assert!(store.is_dir(), "a skipped clone's state is kept: {store:?}");
        }

        // A clone created later under `evals/` is never promoted.
        A9::clone_into(&a.repo, "z_ignore/evals/c3");
        settle(&mut e);
        e.rescan().unwrap();
        assert_eq!(e.root_paths(), vec![a.r.clone(), draft.clone()]);
        drop(e);

        // Restart: identical.
        let mut e = a.open(&a.w, &skip);
        assert_eq!(e.root_paths(), vec![a.r.clone(), draft.clone()]);
        assert_eq!(rows(&mut e, &draft), vec!["notes.md"]);
        assert_eq!(rows(&mut e, &a.r), vec!["t.rs"]);
        drop(e);

        // `skip_globs = ["r"]` under `W`: a level-1 repository, skipped with everything in it.
        let e = a.open(&a.w, &["r"]);
        assert!(e.root_paths().is_empty(), "{:?}", e.root_paths());
        drop(e);

        // Under `parent_dirs = [W/r]` the entry itself is never skipped, and says so.
        let e = a.open(&a.r, &["r"]);
        assert!(e.root_paths().contains(&a.r), "{:?}", e.root_paths());
        assert!(
            e.notices()
                .iter()
                .any(|n| n.contains("skip_globs") && n.contains("ignored")),
            "{:?}",
            e.notices()
        );
    }
}
