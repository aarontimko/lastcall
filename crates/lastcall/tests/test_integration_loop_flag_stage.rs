//! Verifier (b) F1, the end the reducer tests could not reach: **the loop** turns one `m`
//! into `pane.send_text` on the socket.
//!
//! Every Phase 7 send test before this one injected `HerdrUpdate::Agents` by hand
//! (`app_flagged_with_one_agent_stages_without_asking`, the `tui_agent_picker` snapshot) or
//! called `herdr::stage` directly (`herdr_stage_wraps_the_export_in_bracketed_paste_markers`,
//! the real-herdr scene). Nothing proved the loop ever *built* that update, and it did not:
//! `herdr_rederive` sent `Scope` and `Roots` only, so `HerdrView::candidates` was empty in
//! the built binary and every flag fell through to the export file.
//!
//! So this scene runs the production pieces in the production order — a real herdr client
//! over a real socket (the mock), `run::herdr_fold` on the snapshot that client installed,
//! `Ui::event` for the keystrokes, `run::spawn_flag` and `run::spawn_stage` for the effects
//! the pass produced — and asserts the request that lands on the socket.
//!
//! Not a PTY scene on purpose: no PTY scene talks to herdr (deliverable 11), and the
//! bracketed-paste *semantics* are the real-herdr tier's job
//! (`herdr_real_send_text_lands_unsubmitted`). What is pinned here is the wiring between
//! them.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use lastcall::tui::app::{App, Changed, Effect, FlagKind, RootMeta, Selection};
use lastcall::tui::herdr::STAGE_TAIL;
use lastcall::tui::input::Keymap;
use lastcall::tui::run::{FlagRequest, Local, Ui, herdr_fold, spawn_flag, spawn_stage};
use lastcall_engine::herdr::client::{Client, ClientOptions, ClientTimings};
use lastcall_engine::herdr::transport::SocketTransport;
use lastcall_engine::herdr::{Compat, HerdrEvent, guard};
use lastcall_engine::watcher::EngineEvent;
use lastcall_testkit::engine::open_engine;
use lastcall_testkit::fixture_parent;
use lastcall_testkit::fixture_repo::engine_env_for;
use lastcall_testkit::mock_herdr::MockHerdr;
use lastcall_testkit::tmp::TempDir;
use serde_json::json;
use tokio::sync::mpsc;

/// herdr's own public pane id for the one agent under `W/alpha`.
const PANE: &str = "pane_alpha";
const WS: &str = "ws_1";

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// A `session_snapshot` with one agent pane whose `cwd` is `alpha`, one plain shell pane in
/// the same directory (not a candidate), and one agent pane in another root.
fn snapshot(alpha: &str, beta: &str) -> serde_json::Value {
    let pane = |id: &str, cwd: &str, agent: Option<&str>| {
        json!({
            "pane_id": id, "terminal_id": "term_1", "workspace_id": WS, "tab_id": "tab_1",
            "focused": false, "agent_status": "idle", "revision": 0,
            "agent": agent, "cwd": cwd,
        })
    };
    json!({
        "type": "session_snapshot",
        "snapshot": {
            "version": "0.8.2", "protocol": 21,
            "workspaces": [{ "workspace_id": WS, "label": "alpha" }],
            "tabs": [{ "tab_id": "tab_1", "workspace_id": WS, "label": "t" }],
            "panes": [
                pane(PANE, alpha, Some("claude")),
                pane("pane_shell", alpha, None),
                pane("pane_beta", beta, Some("claude")),
            ],
            "layouts": [],
            "agents": [],
        }
    })
}

/// Wait for one `Local` the loop's spawned task sends back.
async fn next_local(rx: &mut mpsc::UnboundedReceiver<Local>) -> Local {
    tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .expect("the spawned task answered")
        .expect("the channel is open")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loop_flag_with_one_agent_reaches_pane_send_text() {
    let w_dir = TempDir::new("lc-loop-flag");
    let state = TempDir::new("lc-loop-flag-state");
    let parent = w_dir.join("W");
    let built = fixture_parent::build(&parent, state.path(), &state.join("home"))
        .expect("the fixture builds");
    let env = engine_env_for(&parent, &built.home, state.path());
    let engine = open_engine(&parent, &env, state.path(), fixture_parent::config());

    // The app the loop would hold after its first refresh: the engine's own roots and its
    // own piles, nothing hand-written.
    let mut engine = engine;
    let metas: Vec<RootMeta> = engine.roots().iter().map(|r| RootMeta::of(r)).collect();
    let alpha: PathBuf = metas
        .iter()
        .find(|m| m.name == "alpha")
        .expect("alpha is a root")
        .path
        .clone();
    let beta: PathBuf = metas
        .iter()
        .find(|m| m.name == "beta")
        .expect("beta is a root")
        .path
        .clone();
    let mut app = App::new();
    app.sync_roots(metas);
    for (root, seq, result) in engine.scan_all() {
        app.apply(EngineEvent::Pile {
            root,
            seq,
            pile: result.expect("the fixture scans"),
        });
    }
    app.handle(lastcall::tui::input::Action::Resize(100, 30));
    let mut ui = Ui::new(app, Keymap::defaults());
    let engine = Arc::new(Mutex::new(engine));

    // A herdr on a real socket, spoken to by the real client — the same setup the loop's
    // connect arm performs.
    let sock_dir = TempDir::socket_dir();
    let sock = sock_dir.join("h.sock");
    let mock = MockHerdr::builder()
        .snapshot(snapshot(&alpha.to_string_lossy(), &beta.to_string_lossy()))
        .canned("pane.send_text", json!({ "type": "ok" }))
        .serve(&sock)
        .await
        .expect("bind the mock socket");
    let transport = SocketTransport::new(&sock, Duration::from_secs(5));
    assert!(
        matches!(guard::probe(&transport).await, Compat::Ok { .. }),
        "the mock passes the protocol guard"
    );
    let (handle, mut events) = Client::spawn(
        transport.clone(),
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
    // The cache is installed by the client's own task, which `Connected` can beat to this
    // one — under the whole integration tier's parallel load it does. Wait for it rather
    // than racing it.
    let cache = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(c) = handle.snapshot() {
                return c;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the client installed a cache");

    // The loop's fold. Without `HerdrUpdate::Agents` (F1) this is where the send dies: the
    // candidate list stays empty and `App::flagged` takes the export-file arm.
    let (changed, _) = herdr_fold(&mut ui, &cache, None, None);
    assert_eq!(changed, Changed::Yes);
    let candidates = ui.app.herdr.candidates(&alpha);
    assert_eq!(
        candidates.len(),
        1,
        "exactly one agent under alpha, so the send asks nothing: {candidates:?}"
    );
    assert_eq!(candidates[0].pane_id, PANE);

    // `m` on alpha's pending file, a note, Enter.
    ui.app
        .select(Some(Selection::Row(alpha.clone(), b"f2".to_vec())));
    let (_, effect) = ui.event(&key(KeyCode::Char('m')));
    assert!(effect.is_none(), "`m` opens the modal, it writes nothing");
    assert!(ui.app.note.is_some(), "the note modal is open");
    for c in "this looks wrong".chars() {
        ui.event(&key(KeyCode::Char(c)));
    }
    let (_, effect) = ui.event(&key(KeyCode::Enter));
    let Some(Effect::Flag {
        root,
        path,
        note,
        hunk,
        summary,
        label,
    }) = effect
    else {
        panic!("Enter sends the flag: {effect:?}");
    };
    assert_eq!(note, "this looks wrong");
    assert_eq!(
        label, "f2",
        "the write carries the words its answer will use"
    );

    // The loop's dispatch for that effect, and its answer.
    let (tx, mut rx) = mpsc::unbounded_channel();
    spawn_flag(
        &engine,
        tx.clone(),
        FlagRequest {
            root,
            path,
            note,
            hunk,
            summary,
            label,
        },
    );
    let flagged = next_local(&mut rx).await;
    assert!(
        matches!(&flagged, Local::Flagged { kind: FlagKind::Flag { label }, result: Ok(_), .. } if label == "f2"),
        "{flagged:?}"
    );
    let (_, effect) = ui.local(flagged);
    let Some(Effect::Stage {
        pane_id,
        flag,
        export,
    }) = effect
    else {
        panic!("one candidate stages without asking: {effect:?}");
    };
    assert_eq!(pane_id, PANE);
    assert!(
        export.contains("this looks wrong"),
        "the export carries the note:\n{export}"
    );

    // …and the dispatch for *that* effect, which is the socket call.
    spawn_stage(Some(transport.clone()), tx, pane_id, flag, export.clone());
    let staged = next_local(&mut rx).await;
    assert!(
        matches!(&staged, Local::Staged { result: Ok(()), .. }),
        "{staged:?}"
    );
    ui.local(staged);

    let sent = mock
        .control()
        .requests()
        .into_iter()
        .find(|r| r.method == "pane.send_text")
        .expect("the loop reached pane.send_text");
    assert_eq!(sent.params["pane_id"], PANE);
    let text = sent.params["text"].as_str().expect("text is a string");
    assert!(
        text.starts_with('\u{1b}') && text[1..].starts_with("[200~"),
        "bracketed paste opens the payload: {text:?}"
    );
    assert!(
        text.ends_with("\u{1b}[201~"),
        "…and closes it, with no keystroke after the paste to submit it: {text:?}"
    );
    assert_eq!(
        &text[6..text.len() - 6],
        format!("{export}{STAGE_TAIL}"),
        "the bytes between the markers are the export the reducer produced plus the \
         separator that keeps the next staged flag on a line of its own"
    );
    assert!(text.contains("this looks wrong"), "{text:?}");

    let status = ui.app.status.as_ref().expect("a status line").text.clone();
    assert_eq!(status, "flagged f2 · staged to claude", "{status}");

    handle.shutdown().await;
    mock.shutdown().await;
}
