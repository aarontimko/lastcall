//! Phase 5 deliverable 7 (integration tier: real git, a real socket, real time). herdr
//! notices a worktree before the filesystem watch can: `Engine::rescan` opens new roots but
//! neither scans nor watches them, so only [`Watcher::request_rescan`] turns a fresh
//! checkout into a visible root. The scene here is the one the TUI loop runs — a
//! `worktree_created` line down the lifecycle stream, [`triggers_rescan`], one
//! [`WORKTREE_DEBOUNCE`], one `request_rescan` — with the discovery backstop parked at five
//! minutes so nothing but the trigger can explain the result.
//!
//! The loop's own timer arm (`run.rs`) is not reachable without a terminal; the PTY tier
//! covers the loop end to end, and what is pinned here is the trigger set, the delay and
//! the engine's answer to it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use lastcall::tui::herdr::triggers_rescan;
use lastcall::tui::run::WORKTREE_DEBOUNCE;
use lastcall_engine::herdr::client::{Client, ClientOptions, ClientTimings, WorktreeChange};
use lastcall_engine::herdr::transport::SocketTransport;
use lastcall_engine::herdr::{Compat, HerdrEvent, guard};
use lastcall_engine::watcher::{EngineEvent, EngineTimings, Watcher};
use lastcall_testkit::engine::open_engine;
use lastcall_testkit::fixture_parent;
use lastcall_testkit::fixture_repo::{FixtureRepo, engine_env_for};
use lastcall_testkit::mock_herdr::MockHerdr;
use lastcall_testkit::tmp::TempDir;

/// Far past every assertion window below: if the new root appears, the trigger is why.
const BACKSTOP: Duration = Duration::from_secs(300);

async fn next_event(w: &mut Watcher, deadline: Duration) -> Option<EngineEvent> {
    tokio::time::timeout(deadline, w.events.recv())
        .await
        .ok()
        .flatten()
}

/// Consume events until the watch is live (registering an FSEvents stream takes seconds on
/// macOS); the gap-closing scans precede the notice.
async fn wait_live(w: &mut Watcher) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no 'watching' notice within 30 s"
        );
        if let Some(EngineEvent::Notice { text, .. }) = next_event(w, Duration::from_secs(5)).await
            && text.starts_with("watching ")
        {
            return;
        }
    }
}

/// One `worktree_created` line in herdr's lifecycle envelope.
fn worktree_created_line(path: &Path, branch: &str) -> String {
    serde_json::json!({
        "event": "worktree_created",
        "data": {
            "workspace": {"workspace_id": "w1", "label": "alpha"},
            "worktree": {
                "path": path.to_string_lossy(),
                "branch": branch,
                "is_linked_worktree": true
            }
        }
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn herdr_worktree_created_rescans_within_the_debounce_not_the_backstop() {
    let w_dir = TempDir::new("lc-wt-parent");
    let state = TempDir::new("lc-wt-state");
    let parent = w_dir.join("W");
    let built = fixture_parent::build(&parent, state.path(), &state.join("home"))
        .expect("the fixture builds");
    let env = engine_env_for(&parent, &built.home, state.path());
    let engine = open_engine(&parent, &env, state.path(), fixture_parent::config());
    let mut watcher = engine.run(EngineTimings {
        debounce: Duration::from_millis(100),
        head_poll: BACKSTOP,
        rescan: BACKSTOP,
        ..EngineTimings::default()
    });
    wait_live(&mut watcher).await;

    // A herdr on a real socket, spoken to by the real client.
    let sock = state.join("herdr.sock");
    let mock = MockHerdr::builder()
        .serve(&sock)
        .await
        .expect("bind the mock socket");
    let transport = SocketTransport::new(&sock, Duration::from_secs(5));
    assert!(
        matches!(guard::probe(&transport).await, Compat::Ok { .. }),
        "the mock passes the protocol guard"
    );
    let (handle, mut events) = Client::spawn(
        transport,
        ClientTimings::default(),
        ClientOptions { reconnect: true },
    );
    let connected = tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("the client connects")
        .expect("an event");
    assert!(
        matches!(connected, HerdrEvent::Connected { .. }),
        "{connected:?}"
    );

    // The user runs `git worktree add` in a herdr pane: a real linked checkout under the
    // same parent dir, with something pending in it.
    let alpha = FixtureRepo::open_in(TempDir::adopt(&parent), "alpha");
    let checkout: PathBuf = parent.join("alpha-wt");
    alpha
        .git(&[
            "worktree",
            "add",
            "-b",
            "wt",
            &checkout.to_string_lossy(),
            "HEAD",
        ])
        .expect("git worktree add");
    std::fs::write(checkout.join("f1"), "worktree edit\n").expect("edit in the new checkout");

    // herdr says so. The client is what parses the line; the loop's rule is the assertion.
    mock.push_lifecycle(worktree_created_line(&checkout, "wt"));
    let pushed = tokio::time::Instant::now();
    let event = loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("a worktree event within 10 s")
            .expect("an event");
        if matches!(event, HerdrEvent::WorktreeChanged { .. }) {
            break event;
        }
    };
    let HerdrEvent::WorktreeChanged { change, path, .. } = &event else {
        unreachable!()
    };
    assert_eq!(*change, WorktreeChange::Created);
    assert_eq!(Path::new(path), checkout);
    assert!(triggers_rescan(&event), "the loop's trigger set covers it");

    // What the loop does with it: coalesce the burst, then ask once.
    tokio::time::sleep(WORKTREE_DEBOUNCE).await;
    watcher.request_rescan();

    let mut named = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while named.is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no RootsChanged naming the new checkout"
        );
        if let Some(EngineEvent::RootsChanged(roots)) =
            next_event(&mut watcher, Duration::from_secs(5)).await
            && let Some(added) = roots.added.iter().find(|r| r.ends_with("alpha-wt"))
        {
            named = Some(added.clone());
        }
    }
    let roots_changed = pushed.elapsed();

    let mut piled = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !piled {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no Pile for the new checkout"
        );
        if let Some(EngineEvent::Pile { root, .. }) =
            next_event(&mut watcher, Duration::from_secs(5)).await
        {
            piled = root.ends_with("alpha-wt");
        }
    }
    let pile = pushed.elapsed();
    assert!(
        pile < BACKSTOP / 10,
        "the trigger, not the backstop: pile after {pile:?}"
    );
    eprintln!(
        "herdr worktree trigger: RootsChanged in {roots_changed:.3?}, Pile in {pile:.3?} \
         (debounce {WORKTREE_DEBOUNCE:?}, backstop {BACKSTOP:?})"
    );

    handle.shutdown().await;
    mock.shutdown().await;
    watcher.join().await;
}
