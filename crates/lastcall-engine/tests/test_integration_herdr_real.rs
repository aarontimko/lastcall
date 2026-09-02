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
use std::time::{Duration, Instant};

use lastcall_engine::herdr::client::{Client, ClientOptions, ClientTimings, HerdrEvent};
use lastcall_engine::herdr::guard;
use lastcall_engine::herdr::transport::{SocketTransport, Transport, TransportError};
use lastcall_engine::herdr::wire::{self, AgentStatus, Event, Subscription};
use lastcall_testkit::herdr_spawn::{SpawnedHerdr, herdr_bin_from_env, write_skip_notice};
use lastcall_testkit::json_lines;
use serde_json::{Value, json};

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
