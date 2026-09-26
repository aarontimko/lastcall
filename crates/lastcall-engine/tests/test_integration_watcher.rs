//! Watcher tests that need real time and a real filesystem watch (seconds of waiting for
//! FSEvents registration, polling backstops): the integration tier, per
//! docs/dev/testing.md ("no sleeps longer than 50 ms in the unit tier"). The pure routing
//! and allowlist tests stay in `watcher.rs`.

use std::path::Path;
use std::time::Duration;

use lastcall_engine::config::Config;
use lastcall_engine::scan::pile_lines;
use lastcall_engine::watcher::{EngineEvent, EngineTimings, Watcher, lock};
use lastcall_testkit::engine::open_engine;
use lastcall_testkit::fixture_repo::FixtureRepo;
use lastcall_testkit::tmp::TempDir;
use notify::{RecursiveMode, Watcher as _};

/// The next event that is not a `Scanned` progress tick: these tests watch piles, heads
/// and notices; the ticks are counted by `watcher_reports_each_root_scanned_before_its_pile`.
async fn next_event(w: &mut Watcher, deadline: Duration) -> Option<EngineEvent> {
    let until = tokio::time::Instant::now() + deadline;
    loop {
        let left = until.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, w.events.recv())
            .await
            .ok()
            .flatten()
        {
            Some(EngineEvent::Scanned { .. }) => continue,
            other => return other,
        }
    }
}

/// Every root is reported `Scanned` before any pile of the initial batch: the TUI's
/// launch hold counts the reports and lists nothing until the last one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_reports_each_root_scanned_before_its_pile() {
    let repo = FixtureRepo::new("watch-scanned").unwrap();
    let state = TempDir::new("lc-watch-state");
    let env = repo.engine_env(state.path());
    let engine = open_engine(repo.parent_dir(), &env, state.path(), Config::default());
    let mut w = engine.run(EngineTimings {
        debounce: Duration::from_millis(100),
        head_poll: Duration::from_millis(250),
        rescan: Duration::from_secs(60),
        ..EngineTimings::default()
    });
    let mut scanned = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no initial pile within 5 s"
        );
        match tokio::time::timeout(Duration::from_secs(5), w.events.recv())
            .await
            .ok()
            .flatten()
        {
            Some(EngineEvent::Scanned { root, .. }) => scanned.push(root),
            Some(EngineEvent::Pile { root, .. }) => {
                assert_eq!(scanned, vec![root], "the root reported before its pile");
                break;
            }
            other => panic!("unexpected before the first pile: {other:?}"),
        }
    }
    w.stop();
}

/// Consume events until the watch is live (registering an FSEvents stream can take
/// seconds on macOS); the gap-closing scans precede the notice.
async fn wait_live(w: &mut Watcher) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no 'watching' notice within 30 s"
        );
        if let Some(EngineEvent::Notice { text, .. }) =
            next_event(&mut *w, Duration::from_secs(5)).await
            && text.starts_with("watching ")
        {
            return;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_commit_without_file_activity_triggers_head_inspection() {
    let repo = FixtureRepo::new("watch-head").unwrap();
    let state = TempDir::new("lc-watch-state");
    let env = repo.engine_env(state.path());
    let engine = open_engine(repo.parent_dir(), &env, state.path(), Config::default());
    let mut w = engine.run(EngineTimings {
        debounce: Duration::from_millis(100),
        head_poll: Duration::from_millis(250),
        rescan: Duration::from_secs(60),
        ..EngineTimings::default()
    });
    // The initial scan.
    let first = next_event(&mut w, Duration::from_secs(5))
        .await
        .expect("initial pile");
    assert!(matches!(first, EngineEvent::Pile { .. }), "{first:?}");
    repo.git(&["commit", "-q", "--allow-empty", "-m", "empty"])
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut seen = None;
    while tokio::time::Instant::now() < deadline {
        match next_event(&mut w, Duration::from_millis(500)).await {
            Some(EngineEvent::Head { notice, branch, .. }) => {
                seen = Some((notice, branch));
                break;
            }
            Some(_) => continue,
            None => continue,
        }
    }
    let (notice, branch) = seen.expect("a Head event within 5s");
    assert_eq!(branch.as_deref(), Some("main"));
    let notice = notice.expect("notice");
    assert!(notice.starts_with("committed on main"), "{notice}");
    w.join().await;
}

/// Whether the platform delivers a filesystem event for a write under `dir` within two
/// seconds (a wedged fseventsd delivers nothing; the watcher then lives on its polling
/// backstops and the delivery test skips with a reason).
fn fs_events_delivered(dir: &Path) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
        let _ = tx.send(res.is_ok());
    }) else {
        return false;
    };
    if w.watch(dir, RecursiveMode::Recursive).is_err() {
        return false;
    }
    std::fs::write(dir.join(".lc-fs-probe"), b"x").expect("probe write");
    let got = rx.recv_timeout(Duration::from_secs(2)).is_ok();
    let _ = std::fs::remove_file(dir.join(".lc-fs-probe"));
    got
}

/// The only test that proves filesystem events are delivered: both polling backstops are
/// out of reach, so the pile after the edit can only come from the watch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_worktree_edit_schedules_a_scan_without_polling() {
    let repo = FixtureRepo::new("watch-edit").unwrap();
    if !fs_events_delivered(repo.path()) {
        use std::io::Write as _;
        let _ = std::io::stderr().write_all(
            b"SKIP: no filesystem event within 2 s here (fseventsd?); the watcher runs on its polling backstops\n",
        );
        return;
    }
    let state = TempDir::new("lc-watch-state");
    let env = repo.engine_env(state.path());
    let engine = open_engine(repo.parent_dir(), &env, state.path(), Config::default());
    let mut w = engine.run(EngineTimings {
        debounce: Duration::from_millis(100),
        head_poll: Duration::from_secs(60),
        rescan: Duration::from_secs(60),
        ..EngineTimings::default()
    });
    let first = next_event(&mut w, Duration::from_secs(5))
        .await
        .expect("initial pile");
    assert!(matches!(first, EngineEvent::Pile { .. }), "{first:?}");
    wait_live(&mut w).await;
    let started = tokio::time::Instant::now();
    repo.write("f1", "edited under watch\n");
    let deadline = started + Duration::from_secs(5);
    let mut shown = None;
    while tokio::time::Instant::now() < deadline {
        if let Some(EngineEvent::Pile { pile, .. }) =
            next_event(&mut w, Duration::from_millis(500)).await
            && pile.row(b"f1").is_some()
        {
            shown = Some(started.elapsed());
            break;
        }
    }
    let took = shown.expect("the edit reached the pile through the watch within 5 s");
    assert!(took < Duration::from_secs(5), "{took:?}");
    w.join().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_ignore_globs_do_not_hide_tracked_edits() {
    let mut repo = FixtureRepo::new("watch-ignore").unwrap();
    repo.commit_files(&[("vendor/x", "v1\n")], "vendor")
        .unwrap();
    let state = TempDir::new("lc-watch-state");
    let env = repo.engine_env(state.path());
    let engine = open_engine(repo.parent_dir(), &env, state.path(), Config::default());
    assert!(engine.ignore_globs().is_match("vendor/x"));
    let root = engine.roots()[0].path.clone();
    let mut w = engine.run(EngineTimings {
        debounce: Duration::from_millis(100),
        head_poll: Duration::from_secs(60),
        rescan: Duration::from_millis(1500),
        ..EngineTimings::default()
    });
    let first = next_event(&mut w, Duration::from_secs(5))
        .await
        .expect("initial pile");
    assert!(
        matches!(first, EngineEvent::Pile { ref pile, .. } if pile.is_empty()),
        "{first:?}"
    );
    wait_live(&mut w).await;

    repo.write("vendor/x", "v2\n");
    // Nothing wakes a scan for an ignored path within the debounce window.
    let quiet = next_event(&mut w, Duration::from_millis(700)).await;
    assert!(quiet.is_none(), "ignored path scheduled a scan: {quiet:?}");

    // A manual rescan still shows the tracked edit ...
    let pile = lock(&w.engine).scan(&root).unwrap();
    assert_eq!(pile_lines(&pile), vec!["vendor/x"]);

    // ... and so does the rescan backstop.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut shown = false;
    while tokio::time::Instant::now() < deadline {
        if let Some(EngineEvent::Pile { pile, .. }) =
            next_event(&mut w, Duration::from_millis(500)).await
            && pile.row(b"vendor/x").is_some()
        {
            shown = true;
            break;
        }
    }
    assert!(shown, "the rescan backstop shows the edit");
    w.join().await;
}

/// The seam Phase 5 deliverable 7 needs (worker 5b): a new root becomes visible when asked,
/// not at the backstop. `Engine::rescan` opens roots but neither scans nor watches them, so
/// this loop is the only thing that turns a new directory into a visible root - and the
/// backstop here is a minute, far past the assertion window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_request_rescan_finds_a_new_root_without_waiting_for_the_backstop() {
    let repo = FixtureRepo::new("watch-req").unwrap();
    let state = TempDir::new("lc-watch-state");
    let env = repo.engine_env(state.path());
    let engine = open_engine(repo.parent_dir(), &env, state.path(), Config::default());
    let mut w = engine.run(EngineTimings {
        debounce: Duration::from_millis(100),
        head_poll: Duration::from_secs(60),
        rescan: Duration::from_secs(60),
        ..EngineTimings::default()
    });
    wait_live(&mut w).await;

    // A second repo under the same parent dir, made after the engine opened.
    let _second = FixtureRepo::new_in(TempDir::adopt(repo.parent_dir()), "later").unwrap();
    w.request_rescan();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no RootsChanged naming the new root within 10 s"
        );
        if let Some(EngineEvent::RootsChanged(roots)) =
            next_event(&mut w, Duration::from_secs(3)).await
            && roots.added.iter().any(|r| r.ends_with("later"))
        {
            break;
        }
    }
    assert!(
        lock(&w.engine)
            .root_paths()
            .iter()
            .any(|p| p.ends_with("later")),
        "the engine holds the new root"
    );
    w.join().await;
}

/// D28 through the loop (Phase 14 C): a listed worktree removed while the watch is live
/// leaves on the removal's own events, well inside the backstop (a minute here), with no
/// `scan failed` and no `head inspection failed` notice on the way. The head poll is short
/// so the `rm -r` form, whose admin dir survives, is inspected several times in the window:
/// each of those is a silent `RootGone`, not a notice.
async fn d28_through_the_loop(rm_r: bool) {
    let mut repo = FixtureRepo::new("R").unwrap();
    if !fs_events_delivered(repo.path()) {
        use std::io::Write as _;
        let _ = std::io::stderr().write_all(
            b"SKIP: no filesystem event within 2 s here (fseventsd?); D28 needs the watch\n",
        );
        return;
    }
    repo.commit_files(&[(".gitignore", ".worktrees/\n")], "ignore worktrees")
        .unwrap();
    repo.git(&["worktree", "add", "-q", ".worktrees/wt", "-b", "feat-w"])
        .unwrap();
    let wt = std::fs::canonicalize(repo.path().join(".worktrees/wt")).unwrap();
    std::fs::write(wt.join("pend"), "p\n").unwrap();
    let state = TempDir::new("lc-watch-state");
    let env = repo.engine_env(state.path());
    let engine = open_engine(
        repo.parent_dir(),
        &env,
        state.path(),
        Config {
            search_depth: 2,
            ..Config::default()
        },
    );
    assert!(
        engine.root_paths().contains(&wt),
        "{:?}",
        engine.root_paths()
    );
    let mut w = engine.run(EngineTimings {
        head_poll: Duration::from_millis(250),
        rescan: Duration::from_secs(60),
        ..EngineTimings::default()
    });
    wait_live(&mut w).await;

    let started = tokio::time::Instant::now();
    if rm_r {
        std::fs::remove_dir_all(&wt).unwrap();
    } else {
        repo.git(&["worktree", "remove", "--force", ".worktrees/wt"])
            .unwrap();
    }
    let mut left = None;
    let mut notices = Vec::new();
    while started.elapsed() < Duration::from_secs(5) {
        match next_event(&mut w, Duration::from_millis(250)).await {
            Some(EngineEvent::RootsChanged(changed)) if changed.removed.contains(&wt) => {
                left.get_or_insert(started.elapsed());
            }
            Some(EngineEvent::Notice { text, .. }) => notices.push(text),
            _ => {}
        }
    }
    let took = left.expect("the removed worktree left the list within 5 s");
    assert!(took < Duration::from_secs(5), "{took:?}");
    eprintln!("D28 (rm -r: {rm_r}): the row left after {took:?}");
    assert!(
        !lock(&w.engine).root_paths().contains(&wt),
        "the engine no longer holds it"
    );
    assert!(
        notices
            .iter()
            .all(|n| !n.starts_with("scan failed") && !n.starts_with("head inspection failed")),
        "{notices:?}"
    );
    w.join().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_d28_a_worktree_removed_by_git_leaves_on_its_own_events() {
    d28_through_the_loop(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_d28_a_worktree_removed_by_rm_r_leaves_on_its_own_events() {
    d28_through_the_loop(true).await;
}

/// Phase 14 H: a repository sitting untracked inside a listed one at launch
/// (`r/evals/c1`) is promoted by the discovery pass inside the launch scan. The loop
/// announces it with a `RootsChanged` well before the backstop (a minute here), its pile
/// arrives, and it is watched: an edit inside it wakes a scan of the clone itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_announces_a_repository_the_launch_scan_found_and_watches_it() {
    let repo = FixtureRepo::new("r").unwrap();
    let clone = repo.path().join("evals").join("c1");
    std::fs::create_dir_all(&clone).unwrap();
    repo.git_at(&clone, &["init", "-q", "-b", "main"]).unwrap();
    std::fs::write(clone.join("x"), "x\n").unwrap();
    repo.git_at(&clone, &["add", "x"]).unwrap();
    repo.git_at(&clone, &["commit", "-qm", "x"]).unwrap();
    let clone = std::fs::canonicalize(&clone).unwrap();
    let state = TempDir::new("lc-watch-state");
    let env = repo.engine_env(state.path());
    let engine = open_engine(repo.parent_dir(), &env, state.path(), Config::default());
    assert!(
        !engine.root_paths().contains(&clone),
        "open lists the repository alone: {:?}",
        engine.root_paths()
    );
    let mut w = engine.run(EngineTimings {
        debounce: Duration::from_millis(100),
        head_poll: Duration::from_secs(60),
        rescan: Duration::from_secs(60),
        ..EngineTimings::default()
    });

    let started = tokio::time::Instant::now();
    let (mut announced, mut piled) = (false, false);
    while !(announced && piled) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "within 10 s (the backstop is a minute): RootsChanged adding the clone {announced}, \
             its pile {piled}"
        );
        match next_event(&mut w, Duration::from_secs(1)).await {
            Some(EngineEvent::RootsChanged(changed)) => {
                assert_eq!(changed.added, vec![clone.clone()], "{changed:?}");
                assert!(changed.removed.is_empty(), "{changed:?}");
                assert!(!changed.reload, "{changed:?}");
                announced = true;
            }
            Some(EngineEvent::Pile { root, .. }) if root == clone => piled = true,
            _ => {}
        }
    }
    assert!(lock(&w.engine).root_paths().contains(&clone));

    if !fs_events_delivered(repo.path()) {
        use std::io::Write as _;
        let _ = std::io::stderr().write_all(
            b"SKIP (second half): no filesystem event within 2 s here (fseventsd?); the clone's watch needs one\n",
        );
        w.join().await;
        return;
    }
    wait_live(&mut w).await;
    let edited = tokio::time::Instant::now();
    std::fs::write(clone.join("x"), "edited under watch\n").unwrap();
    let mut shown = false;
    while edited.elapsed() < Duration::from_secs(5) {
        if let Some(EngineEvent::Pile { root, pile, .. }) =
            next_event(&mut w, Duration::from_millis(500)).await
            && root == clone
            && pile.row(b"x").is_some()
        {
            shown = true;
            break;
        }
    }
    assert!(
        shown,
        "the edit inside the clone woke a scan of the clone within 5 s"
    );
    w.join().await;
}

/// Phase 14 H, the other `scan_all`: the TUI's refresh runs `Engine::scan_all` outside the
/// loop, and a clone its discovery pass promotes is no news to any later `rescan`. The
/// backstop folds `take_roots_changed` into its own pass, so the next one announces it.
/// Every timer is a minute here: only the asked-for pass can carry it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_backstop_announces_a_repository_an_outside_scan_all_found() {
    let repo = FixtureRepo::new("r").unwrap();
    let state = TempDir::new("lc-watch-state");
    let env = repo.engine_env(state.path());
    let engine = open_engine(repo.parent_dir(), &env, state.path(), Config::default());
    let minute = Duration::from_secs(60);
    let mut w = engine.run(EngineTimings {
        debounce: minute,
        debounce_max: minute,
        head_poll: minute,
        rescan: minute,
    });
    wait_live(&mut w).await;

    let clone = repo.path().join("evals").join("c1");
    std::fs::create_dir_all(&clone).unwrap();
    repo.git_at(&clone, &["init", "-q", "-b", "main"]).unwrap();
    let clone = std::fs::canonicalize(&clone).unwrap();
    let results = lock(&w.engine).scan_all();
    assert!(
        results.iter().any(|(p, _, _)| *p == clone),
        "the refresh promoted the clone: {results:?}"
    );
    w.request_rescan();

    let started = tokio::time::Instant::now();
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no RootsChanged naming the clone within 10 s"
        );
        if let Some(EngineEvent::RootsChanged(changed)) =
            next_event(&mut w, Duration::from_secs(1)).await
        {
            assert_eq!(changed.added, vec![clone.clone()], "{changed:?}");
            break;
        }
    }
    w.join().await;
}
