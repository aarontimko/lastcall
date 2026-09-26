//! Phase 14 D (Amendment v1.15): a config reload through the engine and the watcher loop,
//! with real git and a real filesystem watch. The binary's `reload_config` is
//! `config::load` + `Engine::reload` + `RescanTrigger::request_reload`; these tests drive the
//! last two directly with a hand-built `Loaded`, which is what that function hands them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use lastcall_engine::config::{Config, Loaded, Resolved};
use lastcall_engine::engine::{AcceptRequest, Engine};
use lastcall_engine::roots::RootsChanged;
use lastcall_engine::watcher::{EngineEvent, EngineTimings, Watcher, lock};
use lastcall_testkit::engine::{loaded_for, open_engine};
use lastcall_testkit::fixture_repo::FixtureRepo;
use lastcall_testkit::tmp::TempDir;
use notify::{RecursiveMode, Watcher as _};

/// The next event that is not a `Scanned` progress tick, or `None` at the deadline.
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

/// Consume events until `pick` returns `Some`, recording every event seen on the way.
async fn until<T>(
    w: &mut Watcher,
    seen: &mut Vec<EngineEvent>,
    deadline: Duration,
    what: &str,
    mut pick: impl FnMut(&EngineEvent) -> Option<T>,
) -> T {
    let end = tokio::time::Instant::now() + deadline;
    while tokio::time::Instant::now() < end {
        if let Some(ev) = next_event(w, Duration::from_millis(250)).await {
            let got = pick(&ev);
            seen.push(ev);
            if let Some(t) = got {
                return t;
            }
        }
    }
    panic!("{what} within {deadline:?}; saw {seen:#?}");
}

async fn wait_live(w: &mut Watcher) -> String {
    let mut seen = Vec::new();
    until(
        w,
        &mut seen,
        Duration::from_secs(30),
        "the watch live",
        |ev| match ev {
            EngineEvent::Notice { text, .. } if text.starts_with("watching ") => Some(text.clone()),
            _ => None,
        },
    )
    .await
}

/// The reload's own `RootsChanged`.
async fn reload_pass(w: &mut Watcher, seen: &mut Vec<EngineEvent>) -> RootsChanged {
    until(
        w,
        seen,
        Duration::from_secs(20),
        "the reload pass",
        |ev| match ev {
            EngineEvent::RootsChanged(c) if c.reload => Some(c.clone()),
            _ => None,
        },
    )
    .await
}

/// `Engine::reload` under the lock, then the watcher's reload pass: `spawn_reload`'s order.
fn reload(w: &Watcher, loaded: &Loaded, resolved: &Resolved) {
    lock(&w.engine).reload(loaded, resolved);
    w.rescan_trigger().request_reload();
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap()
}

/// A `Loaded`/`Resolved` pair watching exactly `parents`.
fn loaded_with(parents: &[PathBuf], state: &Path, config: Config) -> (Loaded, Resolved) {
    let (mut loaded, mut resolved) = loaded_for(&parents[0], state, config);
    loaded.config.parent_dirs = parents.to_vec();
    resolved.parent_dirs = parents.to_vec();
    (loaded, resolved)
}

fn pile_paths(pile: &lastcall_engine::scan::Pile) -> Vec<String> {
    let mut v: Vec<String> = pile
        .rows
        .iter()
        .map(|r| String::from_utf8_lossy(&r.path).into_owned())
        .collect();
    v.sort();
    v
}

fn ledger_bytes(engine: &Engine, root: &Path) -> Vec<u8> {
    std::fs::read(&engine.root(root).unwrap().paths.ledger).unwrap()
}

/// Re-pointed from `[P1]` to `[P1, P2]`: P2's repository is listed with first sight and
/// P1's ledger is kept byte for byte, so an accept made before the reload still holds. Then
/// back to `[P1]`: P2's repository leaves and its parent is no longer watched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_repoints_parents_keeps_ledgers_and_unwatches_a_removed_parent() {
    let a = FixtureRepo::new("reload-a").unwrap();
    let b = FixtureRepo::new("reload-b").unwrap();
    let (p1, p2) = (canon(a.parent_dir()), canon(b.parent_dir()));
    let (ra, rb) = (canon(a.path()), canon(b.path()));
    b.write("untracked.txt", "b\n");
    let state = TempDir::new("lc-reload-state");
    let env = a.engine_env(state.path());
    let mut engine = open_engine(&p1, &env, state.path(), Config::default());
    assert_eq!(engine.root_paths(), vec![ra.clone()]);

    a.write("accepted.txt", "accepted before the reload\n");
    let pile = engine.scan(&ra).unwrap();
    assert!(pile.row(b"accepted.txt").is_some());
    engine.accept(&ra, AcceptRequest::All(pile)).unwrap();
    assert!(engine.scan(&ra).unwrap().is_empty());
    let ledger = ledger_bytes(&engine, &ra);

    let mut w = engine.run(EngineTimings {
        head_poll: Duration::from_secs(60),
        rescan: Duration::from_secs(60),
        ..EngineTimings::default()
    });
    let first = wait_live(&mut w).await;
    assert!(first.contains(&p1.display().to_string()), "{first}");

    let mut seen = Vec::new();
    let (loaded, resolved) =
        loaded_with(&[p1.clone(), p2.clone()], state.path(), Config::default());
    reload(&w, &loaded, &resolved);
    let changed = reload_pass(&mut w, &mut seen).await;
    assert_eq!((changed.added, changed.removed), (vec![rb.clone()], vec![]));
    let watching = until(
        &mut w,
        &mut seen,
        Duration::from_secs(30),
        "the new watch",
        |ev| match ev {
            EngineEvent::Notice { text, .. } if text.starts_with("watching ") => Some(text.clone()),
            _ => None,
        },
    )
    .await;
    assert!(watching.contains(&p2.display().to_string()), "{watching}");
    {
        let g = lock(&w.engine);
        let b_state = g.root(&rb).expect("P2's repository is listed");
        assert!(
            b_state.ledger.seen_tree.is_some(),
            "first-sighted at HEAD's tree"
        );
        assert_eq!(g.root(&ra).unwrap().parent, p1);
        assert_eq!(ledger_bytes(&g, &ra), ledger, "P1's ledger is untouched");
    }
    let b_ledger = lock(&w.engine).root(&rb).unwrap().paths.ledger.clone();
    // The pass scans the new root before its watch goes live, so its pile may be behind us.
    let b_pile = match seen.iter().find_map(|ev| match ev {
        EngineEvent::Pile { root, pile, .. } if *root == rb => Some(pile.clone()),
        _ => None,
    }) {
        Some(p) => p,
        None => {
            until(
                &mut w,
                &mut seen,
                Duration::from_secs(10),
                "P2's pile",
                |ev| match ev {
                    EngineEvent::Pile { root, pile, .. } if *root == rb => Some(pile.clone()),
                    _ => None,
                },
            )
            .await
        }
    };
    assert_eq!(
        pile_paths(&b_pile),
        vec!["untracked.txt".to_string()],
        "first sight at HEAD: the commit is seen, the untracked file pending"
    );
    assert!(
        lock(&w.engine).scan(&ra).unwrap().is_empty(),
        "the accept made before the reload survives it"
    );

    // Back to P1 alone: the repository leaves, and the watch set loses P2.
    let (loaded, resolved) =
        loaded_with(std::slice::from_ref(&p1), state.path(), Config::default());
    reload(&w, &loaded, &resolved);
    let changed = reload_pass(&mut w, &mut seen).await;
    assert_eq!((changed.added, changed.removed), (vec![], vec![rb.clone()]));
    let watching = until(
        &mut w,
        &mut seen,
        Duration::from_secs(30),
        "the shrunk watch",
        |ev| match ev {
            EngineEvent::Notice { text, .. } if text.starts_with("watching ") => Some(text.clone()),
            _ => None,
        },
    )
    .await;
    assert!(!watching.contains(&p2.display().to_string()), "{watching}");
    assert!(watching.contains("(1 root)"), "{watching}");
    assert_eq!(lock(&w.engine).root_paths(), vec![ra.clone()]);
    assert!(
        b_ledger.is_file(),
        "the record stays where it was: {}",
        b_ledger.display()
    );
    eprintln!("D reload: [P1] -> [P1, P2] -> [P1]; notices: {first:?}, {watching:?}");
    w.join().await;
}

/// Whether the platform delivers a filesystem event for a write under `dir` within two
/// seconds; the event half of the next test skips with a reason where it does not.
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

/// A reload that changes `ignore_globs` and nothing else moves no root, and still reaches
/// the watcher (design review F4): the loop emits the reload's `RootsChanged` with both
/// lists empty, repeats no `watching` notice, and an event under the newly ignored folder
/// wakes no scan, while the next scan still lists the file there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_of_ignore_globs_alone_repoints_the_watcher() {
    let repo = FixtureRepo::new("reload-ignore").unwrap();
    if !fs_events_delivered(repo.path()) {
        use std::io::Write as _;
        let _ = std::io::stderr().write_all(
            b"SKIP: no filesystem event within 2 s here (fseventsd?); an ignored event cannot be told from a missing one\n",
        );
        return;
    }
    let root = canon(repo.path());
    let parent = canon(repo.parent_dir());
    let state = TempDir::new("lc-reload-ignore-state");
    let env = repo.engine_env(state.path());
    let engine = open_engine(&parent, &env, state.path(), Config::default());
    let mut w = engine.run(EngineTimings {
        debounce: Duration::from_millis(100),
        head_poll: Duration::from_secs(60),
        rescan: Duration::from_secs(60),
        ..EngineTimings::default()
    });
    wait_live(&mut w).await;
    let mut seen = Vec::new();

    // Control: before the reload an event there wakes a scan.
    repo.write("scratch/a.txt", "a\n");
    until(
        &mut w,
        &mut seen,
        Duration::from_secs(5),
        "the control pile",
        |ev| match ev {
            EngineEvent::Pile { pile, .. } if pile.row(b"scratch/a.txt").is_some() => Some(()),
            _ => None,
        },
    )
    .await;

    let mut config = Config::default();
    config.ignore_globs.push("scratch/**".into());
    let (loaded, resolved) = loaded_with(std::slice::from_ref(&parent), state.path(), config);
    reload(&w, &loaded, &resolved);
    let changed = reload_pass(&mut w, &mut seen).await;
    assert!(changed.is_empty(), "{changed:?}");
    // The pass marks every root due; let that scan land before the quiet window.
    until(
        &mut w,
        &mut seen,
        Duration::from_secs(5),
        "the pass's scan",
        |ev| match ev {
            EngineEvent::Pile { root: r, .. } if *r == root => Some(()),
            _ => None,
        },
    )
    .await;

    repo.write("scratch/b.txt", "b\n");
    let quiet = next_event(&mut w, Duration::from_millis(1000)).await;
    assert!(
        quiet.is_none(),
        "a newly ignored path woke something: {quiet:?}"
    );
    assert!(
        !seen.iter().any(
            |ev| matches!(ev, EngineEvent::Notice { text, .. } if text.starts_with("watching "))
        ),
        "a reload that moved no root repeats no `watching` notice: {seen:#?}"
    );

    repo.write("c.txt", "c\n");
    let pile = until(
        &mut w,
        &mut seen,
        Duration::from_secs(5),
        "the next pile",
        |ev| match ev {
            EngineEvent::Pile { pile, .. } if pile.row(b"c.txt").is_some() => Some(pile.clone()),
            _ => None,
        },
    )
    .await;
    assert!(
        pile.row(b"scratch/b.txt").is_some(),
        "ignore_globs scope the watcher only: {pile:?}"
    );
    w.join().await;
}

/// Design review F5: a repository a reload files under another parent dir is closed and
/// opened under the new one in the same pass, first-sighted there, reported removed and
/// added; its old record stays where it was.
#[test]
fn reload_reopens_a_repository_filed_under_another_parent() {
    let outer = TempDir::new("lc-reload-move");
    let group = outer.path().join("group");
    let repo = FixtureRepo::new_in(TempDir::adopt(&group), "r").unwrap();
    let (outer_p, group_p, root) = (canon(outer.path()), canon(&group), canon(repo.path()));
    let state = TempDir::new("lc-reload-move-state");
    let env = repo.engine_env(state.path());
    let config = Config {
        search_depth: 2,
        ..Config::default()
    };
    let mut engine = open_engine(&outer_p, &env, state.path(), config.clone());
    assert_eq!(
        engine.root(&root).expect("found at depth 2").parent,
        outer_p
    );
    repo.write("accepted.txt", "under the outer parent\n");
    let pile = engine.scan(&root).unwrap();
    engine.accept(&root, AcceptRequest::All(pile)).unwrap();
    let old_ledger = engine.root(&root).unwrap().paths.ledger.clone();
    let old_bytes = std::fs::read(&old_ledger).unwrap();
    repo.write("later.txt", "written after the accept\n");

    let (loaded, resolved) = loaded_with(std::slice::from_ref(&group_p), state.path(), config);
    engine.reload(&loaded, &resolved);
    let changed = engine.rescan().unwrap();
    assert_eq!(changed.added, vec![root.clone()]);
    assert_eq!(changed.removed, vec![root.clone()]);
    assert!(!changed.reload, "the watcher sets it, not the engine");
    let state_now = engine.root(&root).unwrap();
    assert_eq!(state_now.parent, group_p, "reopened under the new parent");
    assert_ne!(state_now.paths.ledger, old_ledger);
    assert_eq!(
        pile_paths(&engine.scan(&root).unwrap()),
        vec!["accepted.txt".to_string(), "later.txt".to_string()],
        "first-sighted under the new parent at HEAD: the old parent's accept does not follow it"
    );
    assert_eq!(
        std::fs::read(&old_ledger).unwrap(),
        old_bytes,
        "the old record stays where it was"
    );
    // A second pass finds nothing to move.
    assert!(engine.rescan().unwrap().is_empty());
}
