//! Isolated real-herdr integration test (docs/spec/00-spec.md §5.10, §7.2; kickoff
//! deliverable 7). Spawns the **pinned** herdr release (`LASTCALL_TEST_HERDR_BIN`, set by
//! `just test-integration-herdr`) inside a PTY with private config/runtime dirs and every
//! inherited `HERDR_*` removed, then asserts against the real wire.
//!
//! Skips visibly when the variable is unset — the notice is written with `stderr().write_all`
//! because libtest swallows `eprintln!` of passing tests, and `just test-integration` echoes it
//! at the shell level too.
//!
//! With `LASTCALL_RECORD_DIR=<dir>` set (`just herdr-record`), the raw wire lines observed are
//! written there so fixtures can carry recorded provenance.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use lastcall_engine::herdr::client::{Client, ClientOptions, ClientTimings, HerdrEvent};
use lastcall_engine::herdr::guard;
use lastcall_engine::herdr::transport::{EventStream, SocketTransport, Transport, TransportError};
use lastcall_engine::herdr::wire::{self, AgentStatus, Event, Subscription};
use lastcall_testkit::herdr_spawn::{
    HerdrIsolation, SpawnedHerdr, herdr_bin_from_env, write_skip_notice,
};
use lastcall_testkit::json_lines;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

const TIMEOUT: Duration = Duration::from_secs(5);

struct Recorder {
    dir: Option<PathBuf>,
}

impl Recorder {
    fn from_env() -> Self {
        Self {
            dir: std::env::var_os("LASTCALL_RECORD_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
        }
    }

    fn write(&self, name: &str, lines: &[String]) {
        if let Some(dir) = &self.dir {
            std::fs::create_dir_all(dir).expect("create record dir");
            let mut text = lines.join("\n");
            text.push('\n');
            std::fs::write(dir.join(name), text).expect("write recording");
        }
    }

    /// Write the `result` object of a raw response line, pretty-printed (whitespace only).
    fn write_result_pretty(&self, name: &str, raw_line: &str) {
        if self.dir.is_some() {
            let v: Value = serde_json::from_str(raw_line).expect("raw line is JSON");
            let result = v.get("result").cloned().expect("raw line has a result");
            let text = serde_json::to_string_pretty(&result).unwrap();
            self.write(name, &[text]);
        }
    }
}

/// A raw one-shot `session.snapshot` line.
fn raw_snapshot(sock: &Path, id: &str) -> String {
    json_lines::send_request_raw(
        sock,
        &format!(r#"{{"id":"{id}","method":"session.snapshot","params":{{}}}}"#),
        TIMEOUT,
    )
    .expect("raw snapshot")
    .expect("raw snapshot line")
}

fn say(msg: &str) {
    let mut err = std::io::stderr();
    let _ = err.write_all(format!("herdr-real: {msg}\n").as_bytes());
}

/// Collect raw lines from a subscription stream until every `wanted` event name was seen or
/// the deadline passes. Order is never asserted (§5.10: bursts drain across subscriptions).
async fn collect_until(
    stream: &mut lastcall_engine::herdr::transport::EventStream,
    wanted: &[&str],
    deadline: Duration,
) -> (Vec<String>, BTreeSet<String>) {
    let end = Instant::now() + deadline;
    let mut lines = Vec::new();
    let mut seen = BTreeSet::new();
    let mut remaining: BTreeSet<&str> = wanted.iter().copied().collect();
    while !remaining.is_empty() {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match stream.next_line(Some(left)).await {
            Ok(Some(line)) => {
                if let Ok(event) = Event::parse_line(&line) {
                    let name = event.name().to_string();
                    remaining.remove(name.as_str());
                    seen.insert(name);
                }
                lines.push(line);
            }
            Ok(None) => break,
            Err(TransportError::Timeout { .. }) => break,
            Err(err) => panic!("stream error: {err}"),
        }
    }
    (lines, seen)
}

fn status_of(line: &str) -> Option<AgentStatus> {
    match Event::parse_line(line).ok()? {
        Event::PaneAgentStatusChanged(e) => Some(e.agent_status),
        _ => None,
    }
}

#[tokio::test]
async fn herdr_real_ping_bootstrap_events_and_done_derivation() {
    let Some(bin) = herdr_bin_from_env() else {
        write_skip_notice();
        return;
    };
    let recorder = Recorder::from_env();
    let mut herdr = SpawnedHerdr::spawn(&bin).expect("spawn herdr in a PTY");
    herdr
        .wait_for_socket(Duration::from_secs(5))
        .expect("socket appears within 5 s");
    say(&format!(
        "spawned pid {:?} at {}",
        herdr.pid(),
        herdr.socket_path().display()
    ));
    assert!(
        herdr.matcher_accepts(),
        "the PID matcher must recognise the child it spawned (ps -o comm= vs {})",
        herdr.bin().display()
    );
    let sock = herdr.socket_path().to_path_buf();
    assert!(sock.as_os_str().len() < 100, "socket path under 100 bytes");
    let t = SocketTransport::new(&sock, TIMEOUT);

    // 1. ping: version 0.8.2 and a supported protocol. The published v0.8.2 asset answers
    //    protocol 20 (the spec's 21 is herdr master post-release; see guard::SUPPORTED_PROTOCOLS).
    let pong = guard::ping(&t).await.expect("ping");
    assert_eq!(pong.version, "0.8.2", "{pong:?}");
    assert!(
        guard::SUPPORTED_PROTOCOLS.contains(&pong.protocol),
        "{pong:?} not in {:?}",
        guard::SUPPORTED_PROTOCOLS
    );
    assert_eq!(
        pong.protocol, 20,
        "the pinned v0.8.2 release asset answers protocol 20: {pong:?}"
    );
    assert!(guard::compare(&pong).is_ok());
    say(&format!(
        "ping ok: herdr {} protocol {}",
        pong.version, pong.protocol
    ));

    // 2. A raw lifecycle subscription, then workspace.create → snake_case workspace_created.
    let mut life = t
        .subscribe(wire::lifecycle_subscriptions())
        .await
        .expect("lifecycle subscription acknowledged");
    let cwd = herdr.isolation().base.clone();
    let created = t
        .request(
            "workspace.create",
            json!({ "cwd": cwd.to_string_lossy(), "focus": true }),
        )
        .await
        .expect("workspace.create");
    let workspace_id = created["workspace"]["workspace_id"]
        .as_str()
        .expect("workspace_id")
        .to_string();
    let root_pane = created["root_pane"]["pane_id"]
        .as_str()
        .expect("root_pane.pane_id")
        .to_string();
    say(&format!("workspace {workspace_id} root pane {root_pane}"));
    let (mut lifecycle_lines, seen) =
        collect_until(&mut life, &["workspace_created", "pane_created"], TIMEOUT).await;
    assert!(
        seen.contains("workspace_created"),
        "expected a snake_case workspace_created on the lifecycle stream; saw {seen:?}"
    );
    let ws_line = lifecycle_lines
        .iter()
        .find(|l| l.contains("\"workspace_created\""))
        .unwrap();
    let ws_event: Value = serde_json::from_str(ws_line).unwrap();
    assert_eq!(
        ws_event["data"]["type"], "workspace_created",
        "tagged data.type"
    );
    assert_eq!(ws_event["data"]["workspace"]["workspace_id"], workspace_id);
    assert!(ws_event.get("id").is_none(), "pushed events carry no id");

    // 3. Bootstrap through the client: the snapshot has at least one workspace.
    let (handle, mut rx) = Client::spawn(
        t.clone(),
        ClientTimings {
            coalesce: Duration::from_millis(200),
            fallback: Duration::from_secs(30),
            request_timeout: TIMEOUT,
            ..ClientTimings::default()
        },
        ClientOptions { reconnect: false },
    );
    let connected = tokio::time::timeout(TIMEOUT, async {
        loop {
            match rx.recv().await {
                Some(HerdrEvent::Connected { version, protocol }) => break (version, protocol),
                Some(HerdrEvent::Disconnected { reason })
                | Some(HerdrEvent::Standalone { notice: reason }) => {
                    panic!("bootstrap failed: {reason}")
                }
                Some(_) => {}
                None => panic!("client ended before Connected"),
            }
        }
    })
    .await
    .expect("Connected within the deadline");
    assert_eq!(connected, ("0.8.2".to_string(), pong.protocol));
    let cache = handle.snapshot().expect("snapshot installed");
    assert!(
        !cache.workspaces.is_empty(),
        "snapshot must have >= 1 workspace (onboarding = false)"
    );
    assert!(cache.panes.contains_key(&root_pane));
    say(&format!(
        "bootstrap ok: {} workspaces, {} panes",
        cache.workspaces.len(),
        cache.panes.len()
    ));
    recorder.write(
        "snapshot_bootstrap.raw.json",
        &[raw_snapshot(&sock, "rec-snap-0")],
    );
    let first_tab = cache.panes[&root_pane].info.tab_id.clone();

    // 4. done derivation: a second tab takes focus, so the root pane sits in a NON-active
    //    tab; report_agent working → idle there yields a per-pane stream ending in `done`
    //    (herdr derives done from a completion transition in a non-active tab,
    //    src/app/actions.rs:3104-3122 / api_helpers.rs:96-104).
    let tab = t
        .request(
            "tab.create",
            json!({ "workspace_id": workspace_id, "focus": true }),
        )
        .await
        .expect("tab.create");
    assert_eq!(tab["type"], "tab_created");
    let mut status = t
        .subscribe(vec![Subscription::pane_agent_status_changed(&root_pane)])
        .await
        .expect("per-pane status subscription acknowledged");
    for state in ["working", "idle"] {
        let ok = t
            .request(
                "pane.report_agent",
                json!({ "pane_id": root_pane, "source": "lastcall-test", "agent": "demo", "state": state }),
            )
            .await
            .unwrap_or_else(|e| panic!("pane.report_agent {state}: {e}"));
        assert_eq!(ok["type"], "ok");
        if state == "working" {
            // Let herdr observe the working state before the completion transition, and
            // record the two-pane snapshot at this moment: p1 agent-bearing and working in the
            // non-active tab, p2 a bare shell in the active tab (fixture snapshot_two_panes).
            tokio::time::sleep(Duration::from_millis(300)).await;
            recorder.write_result_pretty(
                "snapshot_two_panes.json",
                &raw_snapshot(&sock, "rec-snap-working"),
            );
        }
    }
    let end = Instant::now() + Duration::from_secs(10);
    let mut status_lines = Vec::new();
    let mut statuses = Vec::new();
    while Instant::now() < end {
        let left = end.saturating_duration_since(Instant::now());
        match status.next_line(Some(left)).await {
            Ok(Some(line)) => {
                let parsed: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(
                    parsed["event"], "pane.agent_status_changed",
                    "dotted per-pane event name: {line}"
                );
                assert!(
                    parsed["data"].get("type").is_none(),
                    "per-pane data is untagged: {line}"
                );
                if let Some(s) = status_of(&line) {
                    statuses.push(s);
                }
                status_lines.push(line);
                if statuses.last() == Some(&AgentStatus::Done) {
                    break;
                }
            }
            Ok(None) => panic!("status stream closed early; saw {statuses:?}"),
            Err(TransportError::Timeout { .. }) => break,
            Err(err) => panic!("status stream error: {err}"),
        }
    }
    say(&format!("per-pane statuses: {statuses:?}"));
    assert!(
        statuses.contains(&AgentStatus::Working),
        "expected working on the per-pane stream; saw {statuses:?}"
    );
    assert_eq!(
        statuses.last(),
        Some(&AgentStatus::Done),
        "herdr must derive `done` for a completion in a non-active tab; saw {statuses:?}"
    );
    recorder.write("status_working_to_done.jsonl", &status_lines);
    let pane = t
        .request("pane.get", json!({ "pane_id": root_pane }))
        .await
        .expect("pane.get");
    assert_eq!(
        pane["pane"]["agent_status"], "done",
        "pane.get agrees: {pane}"
    );
    assert_eq!(pane["pane"]["agent"], "demo");

    // The client saw the same transition through its own status subscription (opened from
    // the lifecycle pane_agent_detected / resync path) or through a resync.
    let client_saw_done = tokio::time::timeout(TIMEOUT, async {
        loop {
            match rx.recv().await {
                Some(HerdrEvent::AgentStatusChanged { pane_id, to, .. })
                    if pane_id == root_pane && to == AgentStatus::Done =>
                {
                    break true;
                }
                Some(_) => {}
                None => break false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        client_saw_done,
        "the client must report the working → done transition"
    );
    say("client reported done");

    // 4b. The silent done → idle flip (§5.7): focusing the agent's tab clears herdr's seen
    //     flag with NO global event for the flip itself; the client catches it through the
    //     tab_focused-driven snapshot resync. This is the [sponsor] gate scenario, on the
    //     real binary.
    let focused = t
        .request("tab.focus", json!({ "tab_id": first_tab }))
        .await
        .expect("tab.focus");
    // (v0.8.2 answers `tab_info`; any non-error result is fine.)
    assert!(focused.get("type").is_some(), "{focused}");
    let flip = tokio::time::timeout(TIMEOUT, async {
        loop {
            match rx.recv().await {
                Some(HerdrEvent::AgentStatusChanged {
                    pane_id, from, to, ..
                }) if pane_id == root_pane => break Some((from, to)),
                Some(_) => {}
                None => break None,
            }
        }
    })
    .await
    .expect("a status change after tab.focus within the deadline");
    assert_eq!(
        flip,
        Some((Some(AgentStatus::Done), AgentStatus::Idle)),
        "focusing the tab must surface done → idle through the focus resync"
    );
    let pane = t
        .request("pane.get", json!({ "pane_id": root_pane }))
        .await
        .expect("pane.get after focus");
    assert_eq!(pane["pane"]["agent_status"], "idle", "{pane}");
    recorder.write_result_pretty(
        "snapshot_two_panes_after_focus.json",
        &raw_snapshot(&sock, "rec-snap-idle"),
    );
    say("client reported done -> idle after tab.focus");

    // 4c. pane.close on the second pane: a snake_case pane_closed on the lifecycle stream.
    let second_pane = tab["root_pane"]["pane_id"]
        .as_str()
        .expect("tab_created.root_pane.pane_id")
        .to_string();
    let closed = t
        .request("pane.close", json!({ "pane_id": second_pane }))
        .await
        .expect("pane.close");
    assert!(closed.get("type").is_some(), "{closed}");
    let (more, seen) = collect_until(&mut life, &["pane_closed"], TIMEOUT).await;
    lifecycle_lines.extend(more);
    assert!(seen.contains("pane_closed"), "saw {seen:?}");
    let (more, _) = collect_until(&mut life, &["never_arrives"], Duration::from_millis(300)).await;
    lifecycle_lines.extend(more);
    recorder.write("lifecycle.jsonl", &lifecycle_lines);

    // 5. A bogus subscription set: one error line, then the connection is closed.
    let bogus_raw = json_lines::send_request_raw(
        &sock,
        r#"{"id":"rec-bogus","method":"events.subscribe","params":{"subscriptions":[{"type":"bogus.event"}]}}"#,
        TIMEOUT,
    )
    .expect("raw bogus subscribe")
    .expect("one error line");
    let bogus: Value = serde_json::from_str(&bogus_raw).unwrap();
    assert!(bogus.get("error").is_some(), "{bogus_raw}");
    recorder.write("subscribe_failure.jsonl", std::slice::from_ref(&bogus_raw));
    let err = t
        .subscribe(vec![Subscription::global("bogus.event")])
        .await
        .expect_err("bogus subscription must be refused");
    assert!(
        matches!(err, TransportError::SubscribeRefused(_)),
        "refused, connection closed: {err:?}"
    );
    // The closed connection: a raw reader sees EOF right after the error line.
    let mut raw = json_lines::open_subscription(
        &sock,
        r#"{"id":"rec-bogus2","method":"events.subscribe","params":{"subscriptions":[{"type":"bogus.event"}]}}"#,
    )
    .unwrap();
    let first = raw.read_raw_line(TIMEOUT).unwrap().expect("error line");
    assert!(first.contains("\"error\""));
    let after = raw.read_raw_line(Duration::from_secs(2));
    assert!(
        matches!(&after, Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof),
        "connection must close after the error line, got {after:?}"
    );

    // 6. A per-pane status subscription for a nonexistent pane: pane_not_found.
    let err = t
        .subscribe(vec![Subscription::pane_agent_status_changed("ws_nope:p9")])
        .await
        .expect_err("unknown pane must be refused");
    assert!(err.is_pane_not_found(), "{err:?}");
    assert!(matches!(err, TransportError::SubscribeRefused(_)));
    say("subscription refusals ok");

    // 7. The lifecycle stream is still alive after all that (one-shot failures never touch it).
    let _ = t
        .request(
            "tab.create",
            json!({ "workspace_id": workspace_id, "focus": true }),
        )
        .await
        .expect("tab.create again");
    let (_, seen) = collect_until(&mut life, &["tab_focused"], TIMEOUT).await;
    assert!(
        seen.contains("tab_focused"),
        "lifecycle stream still streams; saw {seen:?}"
    );
    // The client's own view converged too: p2 is gone after its pane_closed-driven resync.
    let converged = tokio::time::timeout(TIMEOUT, async {
        loop {
            if handle
                .snapshot()
                .is_some_and(|c| !c.panes.contains_key(&second_pane))
            {
                break true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        converged,
        "the client's cache drops the closed pane after a resync"
    );

    handle.shutdown().await;
    drop(status);
    drop(life);
    drop(herdr);
    assert!(
        !Path::new(&sock).exists(),
        "the spawned herdr's dir is removed on drop"
    );
    say("done");
}

// ---------------------------------------------------------------------------------------
// Phase 5 deliverable 10 — G3 and G6 against the pinned real server
// ---------------------------------------------------------------------------------------

/// The lines [`FilteringTransport`] swallows.
///
/// Lifecycle events arrive snake_case (§5.5); the per-pane status event arrives dotted.
/// Between them these four are every route by which the client could hear about a focus
/// change or a status change *as an event*, so what is left is the fallback resync, and only
/// the fallback resync.
const FILTERED_EVENTS: [&str; 4] = [
    "tab_focused",
    "pane_focused",
    "workspace_focused",
    wire::PANE_AGENT_STATUS_CHANGED,
];

/// What the filter did, for the assertions.
#[derive(Debug, Default)]
struct FilterStats {
    /// Subscriptions carrying the §5.4 lifecycle set: a second one means a reconnect.
    lifecycle_subscribes: AtomicUsize,
    /// Event names dropped, in arrival order.
    dropped: Mutex<Vec<String>>,
    /// Event names forwarded, in arrival order — named, not just counted, so a heal that was
    /// really driven by some *other* event cannot pass as a fallback resync.
    forwarded: Mutex<Vec<String>>,
}

impl FilterStats {
    fn dropped_names(&self) -> Vec<String> {
        self.dropped
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn forwarded_names(&self) -> Vec<String> {
        self.forwarded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// The name of a pushed event line, either envelope shape.
fn event_name(line: &str) -> Option<String> {
    serde_json::from_str::<Value>(line)
        .ok()?
        .get("event")?
        .as_str()
        .map(str::to_owned)
}

/// A [`Transport`] that deprives the client of the focus and per-pane status lines.
///
/// Requests pass straight through (the client still resyncs over the real socket); only the
/// **event streams** are censored. Every subscription is relayed to herdr exactly as asked —
/// the server never sees a difference — and the acknowledged stream is re-served through an
/// in-process pipe with [`FILTERED_EVENTS`] removed.
///
/// This is what makes G3 a real test: without it the `done` → `idle` flip is announced by
/// `tab_focused`, and a reconnect would re-bootstrap from `session.snapshot` and show idle
/// trivially. With it, the only thing that can heal the cache is the periodic resync.
#[derive(Clone)]
struct FilteringTransport<T: Transport + Clone> {
    inner: T,
    stats: Arc<FilterStats>,
}

impl<T: Transport + Clone> FilteringTransport<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            stats: Arc::new(FilterStats::default()),
        }
    }
}

impl<T: Transport + Clone> Transport for FilteringTransport<T> {
    async fn request(&self, method: &str, params: Value) -> Result<Value, TransportError> {
        self.inner.request(method, params).await
    }

    async fn subscribe(
        &self,
        subscriptions: Vec<Subscription>,
    ) -> Result<EventStream, TransportError> {
        if subscriptions.iter().any(|s| s.kind == "tab.focused") {
            self.stats
                .lifecycle_subscribes
                .fetch_add(1, Ordering::SeqCst);
        }
        let mut inner = self.inner.subscribe(subscriptions).await?;
        let (ours, mut theirs) = tokio::io::duplex(64 * 1024);
        let stats = Arc::clone(&self.stats);
        tokio::spawn(async move {
            while let Ok(Some(line)) = inner.next_line(None).await {
                let name = event_name(&line);
                match &name {
                    Some(name) if FILTERED_EVENTS.contains(&name.as_str()) => {
                        stats
                            .dropped
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(name.clone());
                        continue;
                    }
                    _ => {}
                }
                stats
                    .forwarded
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(name.unwrap_or_else(|| "(no event field)".to_string()));
                if theirs.write_all(line.as_bytes()).await.is_err()
                    || theirs.write_all(b"\n").await.is_err()
                {
                    break;
                }
            }
            // Dropping `theirs` is the EOF the consumer sees: a real close stays a close.
        });
        Ok(EventStream::new(Box::new(ours)))
    }

    fn describe(&self) -> String {
        format!("filtered({})", self.inner.describe())
    }
}

/// Drive the root pane of a fresh workspace to `done`, the way section 4 of the Phase 1 test
/// does: a second tab takes focus, so the completion happens in a tab the user is not
/// viewing and herdr derives `done` (§5.7). Returns `(workspace_id, root_pane, first_tab)`.
async fn drive_pane_to_done<T: Transport>(t: &T, cwd: &Path) -> (String, String, String) {
    let created = t
        .request(
            "workspace.create",
            json!({ "cwd": cwd.to_string_lossy(), "focus": true }),
        )
        .await
        .expect("workspace.create");
    let workspace_id = created["workspace"]["workspace_id"]
        .as_str()
        .expect("workspace_id")
        .to_string();
    let root_pane = created["root_pane"]["pane_id"]
        .as_str()
        .expect("root_pane.pane_id")
        .to_string();
    let first_tab = created["root_pane"]["tab_id"]
        .as_str()
        .expect("root_pane.tab_id")
        .to_string();
    t.request(
        "tab.create",
        json!({ "workspace_id": workspace_id, "focus": true }),
    )
    .await
    .expect("tab.create takes focus away from the agent's tab");
    for state in ["working", "idle"] {
        let ok = t
            .request(
                "pane.report_agent",
                json!({ "pane_id": root_pane, "source": "lastcall-test", "agent": "demo", "state": state }),
            )
            .await
            .unwrap_or_else(|e| panic!("pane.report_agent {state}: {e}"));
        assert_eq!(ok["type"], "ok");
        if state == "working" {
            // Let herdr observe `working` before the completion transition.
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
    // Poll the authority rather than sleeping on hope.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let pane = t
            .request("pane.get", json!({ "pane_id": root_pane }))
            .await
            .expect("pane.get");
        if pane["pane"]["agent_status"] == "done" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "herdr never derived `done` for {root_pane}: {pane}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (workspace_id, root_pane, first_tab)
}

/// **G3** (`01-scenarios.md` §G). The silent `done` → `idle` flip (§5.7) heals through the
/// periodic resync alone.
///
/// The client runs on a [`FilteringTransport`], so the `tab_focused` line that would have
/// driven a focus resync — and every per-pane `agent_status_changed` line — never reaches
/// it. The cache is stale from the moment the tab is focused until the next `fallback`
/// resync, and the test asserts it heals inside `fallback + 1 s` **without a reconnect**
/// (exactly one `Connected`, exactly one lifecycle subscription): a reconnect re-bootstraps
/// from `session.snapshot` and would show idle trivially.
#[tokio::test]
async fn herdr_real_done_flip_heals_within_fallback() {
    let Some(bin) = herdr_bin_from_env() else {
        write_skip_notice();
        return;
    };
    let mut herdr = SpawnedHerdr::spawn(&bin).expect("spawn herdr in a PTY");
    herdr
        .wait_for_socket(Duration::from_secs(5))
        .expect("socket appears within 5 s");
    let sock = herdr.socket_path().to_path_buf();
    say(&format!(
        "G3: spawned pid {:?} at {}",
        herdr.pid(),
        sock.display()
    ));
    let raw = SocketTransport::new(&sock, TIMEOUT);

    // 1. A pane herdr calls `done`, before the client ever connects: the bootstrap snapshot
    //    is the only thing that tells the client so.
    let (_ws, root_pane, first_tab) = drive_pane_to_done(&raw, &herdr.isolation().base).await;
    say(&format!("G3: {root_pane} is done in tab {first_tab}"));

    // 2. Connect through the filter, with a 3 s fallback resync.
    const FALLBACK: Duration = Duration::from_secs(3);
    let filtered = FilteringTransport::new(raw.clone());
    let stats = Arc::clone(&filtered.stats);
    let (handle, mut rx) = Client::spawn(
        filtered,
        ClientTimings {
            coalesce: Duration::from_millis(200),
            fallback: FALLBACK,
            request_timeout: TIMEOUT,
            reconnect_initial: Duration::from_millis(200),
            reconnect_max: Duration::from_secs(1),
        },
        // Reconnect stays ON: a reconnect is the failure this test must be able to see.
        ClientOptions { reconnect: true },
    );
    let mut trace: Vec<String> = Vec::new();
    let mut connects = 0usize;
    tokio::time::timeout(TIMEOUT, async {
        loop {
            match rx.recv().await {
                Some(HerdrEvent::Connected { version, protocol }) => {
                    connects += 1;
                    trace.push(format!("Connected {version} protocol {protocol}"));
                    break;
                }
                Some(HerdrEvent::Standalone { notice }) => panic!("standalone: {notice}"),
                Some(other) => trace.push(format!("{other:?}")),
                None => panic!("client ended before Connected"),
            }
        }
    })
    .await
    .expect("Connected within the deadline");
    let cache = handle.snapshot().expect("bootstrap cache");
    assert_eq!(
        cache.status_of(&root_pane),
        Some(&AgentStatus::Done),
        "the bootstrap snapshot must show the pane as done"
    );

    // 3. Focus the agent's tab. herdr clears its seen flag and the pane becomes idle with no
    //    global event for the flip itself; the one line that *would* have driven a resync
    //    (`tab_focused`) is eaten by the filter.
    // Let the scene settle first. herdr announces the second tab's pane asynchronously, well
    // after `tab.create` has returned, so a `pane_created` can still be in flight here — and
    // any lifecycle event schedules a coalesced resync, which would heal the cache in ~200 ms
    // and make the fallback window irrelevant. Wait for the push stream to go quiet (shorter
    // than one fallback window, so this cannot silently consume the heal itself).
    let settle_deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < settle_deadline {
        match tokio::time::timeout(Duration::from_millis(800), rx.recv()).await {
            Err(_) => break, // quiet
            Ok(Some(event)) => trace.push(format!("settle: {event:?}")),
            Ok(None) => panic!("client ended while settling"),
        }
    }
    assert_eq!(
        handle
            .snapshot()
            .and_then(|c| c.status_of(&root_pane).cloned()),
        Some(AgentStatus::Done),
        "the pane must still read done when the tab is focused, or there is nothing to heal"
    );
    let forwarded_before_focus = stats.forwarded_names().len();
    let focus_at = Instant::now();
    let focused = raw
        .request("tab.focus", json!({ "tab_id": first_tab }))
        .await
        .expect("tab.focus");
    assert!(focused.get("type").is_some(), "{focused}");

    // 4. The heal must arrive within one fallback window plus a second of slack.
    let budget = FALLBACK + Duration::from_secs(1);
    let healed = tokio::time::timeout(budget, async {
        loop {
            match rx.recv().await {
                Some(HerdrEvent::Connected { .. }) => {
                    connects += 1;
                    trace.push("Connected (RECONNECT)".into());
                }
                Some(HerdrEvent::AgentStatusChanged {
                    pane_id, from, to, ..
                }) if pane_id == root_pane && to == AgentStatus::Idle => {
                    trace.push(format!("AgentStatusChanged {pane_id} {from:?} -> idle"));
                    break true;
                }
                Some(HerdrEvent::Resync(target)) => trace.push(format!("Resync({target:?})")),
                Some(HerdrEvent::Disconnected { reason }) => {
                    trace.push(format!("Disconnected {reason}"));
                }
                Some(other) => trace.push(format!("{other:?}")),
                None => break false,
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "no idle within {budget:?}; trace:\n  {}",
            trace.join("\n  ")
        )
    });
    let elapsed = focus_at.elapsed();
    say(&format!("G3 trace ({elapsed:?} after tab.focus):"));
    for line in &trace {
        say(&format!("  {line}"));
    }
    assert!(healed, "the client channel ended before the heal");
    assert!(
        elapsed <= budget,
        "heal took {elapsed:?}, over the {budget:?} budget"
    );

    // 5. It healed *through the resync*, not through an event the filter was meant to eat.
    let dropped = stats.dropped_names();
    say(&format!("G3 dropped {} lines: {dropped:?}", dropped.len()));
    assert!(
        dropped.iter().any(|n| n == "tab_focused"),
        "the filter must have eaten the tab_focused announcing the flip; dropped {dropped:?}"
    );
    assert!(
        trace.iter().any(|l| l.starts_with("Resync(")),
        "the heal must be preceded by a resync; trace: {trace:?}"
    );

    // 6. And it healed without a reconnect: one Connected, one lifecycle subscription.
    assert_eq!(
        connects, 1,
        "a reconnect would make this test vacuous; trace: {trace:?}"
    );
    assert_eq!(
        stats.lifecycle_subscribes.load(Ordering::SeqCst),
        1,
        "exactly one lifecycle stream was ever opened"
    );
    assert!(
        !trace.iter().any(|l| l.starts_with("Disconnected")),
        "no disconnect; trace: {trace:?}"
    );
    let forwarded = stats.forwarded_names();
    assert!(
        !forwarded.is_empty(),
        "the filter must still have forwarded the lines it does not censor"
    );
    let after_focus = &forwarded[forwarded_before_focus.min(forwarded.len())..];
    say(&format!(
        "G3 forwarded {} lines ({} after tab.focus: {after_focus:?})",
        forwarded.len(),
        after_focus.len()
    ));
    assert!(
        after_focus.is_empty(),
        "the heal must be the fallback resync and nothing else, but herdr pushed \
         {after_focus:?} between the focus and the heal — either the filter list in the \
         kickoff is incomplete or herdr announces the flip another way (report it)"
    );

    // 7. The cache agrees with the server.
    assert_eq!(
        handle
            .snapshot()
            .and_then(|c| c.status_of(&root_pane).cloned()),
        Some(AgentStatus::Idle)
    );
    let pane = raw
        .request("pane.get", json!({ "pane_id": root_pane }))
        .await
        .expect("pane.get after focus");
    assert_eq!(pane["pane"]["agent_status"], "idle", "{pane}");
    handle.shutdown().await;
    say("G3 done");
}

/// **G6** (`01-scenarios.md` §G). A real server restart converges: the client reconnects to
/// the **same socket** and its cache equals a fresh `session.snapshot` from the new process.
///
/// The first server is stopped with `herdr server stop` over its own isolated socket, not a
/// signal, so the socket file is gone deterministically (a stale socket would make the
/// client see a connect refusal rather than a clean disconnect).
#[tokio::test]
async fn herdr_real_disconnect_reconnect_converges() {
    let Some(bin) = herdr_bin_from_env() else {
        write_skip_notice();
        return;
    };
    let mut herdr = SpawnedHerdr::spawn(&bin).expect("spawn herdr in a PTY");
    herdr
        .wait_for_socket(Duration::from_secs(5))
        .expect("socket appears within 5 s");
    let sock = herdr.socket_path().to_path_buf();
    let isolation: HerdrIsolation = herdr.isolation().clone();
    isolation
        .assert_isolated()
        .expect("the isolation is private before anything is stopped");
    say(&format!(
        "G6: spawned pid {:?} at {}",
        herdr.pid(),
        sock.display()
    ));
    let t = SocketTransport::new(&sock, TIMEOUT);
    // Some state to converge on, so an empty cache cannot pass for a converged one.
    let (_ws, root_pane, _tab) = drive_pane_to_done(&t, &isolation.base).await;

    let (handle, mut rx) = Client::spawn(
        t.clone(),
        ClientTimings {
            coalesce: Duration::from_millis(200),
            // Parked far past the test: every resync counted below is a bootstrap.
            fallback: Duration::from_secs(300),
            request_timeout: TIMEOUT,
            reconnect_initial: Duration::from_millis(200),
            reconnect_max: Duration::from_secs(1),
        },
        ClientOptions { reconnect: true },
    );
    let mut trace: Vec<String> = Vec::new();
    let first_connect = tokio::time::timeout(TIMEOUT, async {
        loop {
            match rx.recv().await {
                Some(HerdrEvent::Connected { version, protocol }) => break (version, protocol),
                Some(HerdrEvent::Standalone { notice }) => panic!("standalone: {notice}"),
                Some(other) => trace.push(format!("{other:?}")),
                None => panic!("client ended before Connected"),
            }
        }
    })
    .await
    .expect("Connected within the deadline");
    trace.push(format!(
        "Connected {} protocol {}",
        first_connect.0, first_connect.1
    ));
    let before = handle.snapshot().expect("first cache");
    assert_eq!(before.resyncs, 1, "bootstrap is the first resync");
    assert!(before.panes.contains_key(&root_pane));

    // 1. Stop it the way a user does. `herdr server stop` waits for the socket to go away.
    let stop_at = Instant::now();
    herdr
        .stop_server(Duration::from_secs(10))
        .expect("`herdr server stop` over the isolated socket");
    assert!(
        !sock.exists(),
        "the socket file must be gone after `server stop`"
    );
    say(&format!("G6: stopped in {:?}", stop_at.elapsed()));

    // 2. The client notices.
    let reason = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Some(HerdrEvent::Disconnected { reason }) => break Some(reason),
                Some(other) => trace.push(format!("{other:?}")),
                None => break None,
            }
        }
    })
    .await
    .expect("a Disconnected within 10 s")
    .expect("the channel stays open across a disconnect");
    trace.push(format!("Disconnected {reason}"));
    say(&format!("G6: disconnected: {reason}"));

    // 3. Same socket path, same config dir — anything else would not be a reconnect.
    herdr
        .respawn(&isolation)
        .expect("respawn on the same socket");
    herdr
        .wait_for_socket(Duration::from_secs(10))
        .expect("the new server's socket appears");
    assert_eq!(herdr.socket_path(), sock.as_path());
    say(&format!("G6: respawned pid {:?}", herdr.pid()));

    // 4. Full bootstrap again.
    let second_connect = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match rx.recv().await {
                Some(HerdrEvent::Connected { version, protocol }) => break (version, protocol),
                Some(HerdrEvent::Standalone { notice }) => panic!("standalone: {notice}"),
                Some(other) => trace.push(format!("{other:?}")),
                None => panic!("client ended before it reconnected"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no reconnect; trace:\n  {}", trace.join("\n  ")));
    trace.push(format!(
        "Connected {} protocol {}",
        second_connect.0, second_connect.1
    ));
    assert_eq!(second_connect, first_connect, "same herdr identity");

    // 5. `Cache.resyncs` + 1, and the cache equals a fresh snapshot from the NEW server.
    let fresh: Value =
        serde_json::from_str(&raw_snapshot(&sock, "g6-fresh")).expect("raw snapshot parses");
    let fresh = &fresh["result"]["snapshot"];
    let fresh_panes: BTreeSet<String> = fresh["panes"]
        .as_array()
        .expect("panes[]")
        .iter()
        .map(|p| p["pane_id"].as_str().expect("pane_id").to_string())
        .collect();
    let fresh_workspaces: BTreeSet<String> = fresh["workspaces"]
        .as_array()
        .expect("workspaces[]")
        .iter()
        .map(|w| {
            w["workspace_id"]
                .as_str()
                .expect("workspace_id")
                .to_string()
        })
        .collect();
    let after = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(c) = handle.snapshot()
                && c.resyncs > before.resyncs
                && c.panes.keys().cloned().collect::<BTreeSet<_>>() == fresh_panes
            {
                break c;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "cache never converged on the new server's snapshot ({fresh_panes:?}); trace:\n  {}",
            trace.join("\n  ")
        )
    });
    assert_eq!(
        after.resyncs,
        before.resyncs + 1,
        "one more resync: the reconnect's bootstrap"
    );
    assert_eq!(
        after.workspaces.keys().cloned().collect::<BTreeSet<_>>(),
        fresh_workspaces
    );
    assert_eq!(after.version, first_connect.0);
    assert_eq!(after.protocol, first_connect.1);
    assert_eq!(
        after.focused_workspace_id.as_deref(),
        fresh["focused_workspace_id"].as_str()
    );
    say("G6 trace:");
    for line in &trace {
        say(&format!("  {line}"));
    }
    say(&format!(
        "G6: converged, resyncs {} -> {}, {} panes, {} workspaces",
        before.resyncs,
        after.resyncs,
        after.panes.len(),
        after.workspaces.len()
    ));
    handle.shutdown().await;
    say("G6 done");
}
