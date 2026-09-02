//! A scripted mock herdr server implementing the *transport contract* (docs/spec/00-spec.md
//! §5.1, §5.3, §5.5): one request per connection, `params` required, `{"id","result"}` /
//! `{"id","error"}` envelopes, `events.subscribe` acks with `subscription_started` then
//! streams, per-pane `pane.agent_status_changed` requires `pane_id` and answers
//! `pane_not_found` then closes for unknown panes, and a subscription-set construction failure
//! is one error line then close — no partial streams.
//!
//! Two faces over one core:
//!
//! - [`MockHerdr`]: bound to a real Unix socket path (`MockHerdrBuilder::serve`). Used by the
//!   transport tests (real time) and by the `mock_herdr` example behind `just probe-hello`.
//! - [`InMemoryHerdr`]: an in-memory [`Transport`] over tokio duplex streams
//!   (`MockHerdrBuilder::in_memory`). Used by the client state-machine unit tests under
//!   `tokio::time::pause` — a paused clock auto-advances whenever the runtime idles on a real
//!   socket read, which would fire the coalesce window, fallback timer, and request timeouts
//!   "instantly"; duplex streams never idle the runtime that way.
//!
//! Both record every request (method, params, order) and every subscription set, and both
//! expose the same control surface (`set_snapshot`, `push_lifecycle`, `push_status`,
//! server-side closes, open-stream counters) so a test can drive any scenario.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lastcall_engine::herdr::transport::{
    EventStream, Transport, TransportError, interpret_ack, interpret_response,
};
use lastcall_engine::herdr::wire::{
    self, ErrorBody, PANE_AGENT_STATUS_CHANGED, Request, SUBSCRIPTION_STARTED, Subscription,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Every subscription `type` herdr v0.8.2 accepts, verbatim from the error message the
/// release emits for an unknown one (`fixtures/herdr/recorded/subscribe_failure.jsonl`).
pub const KNOWN_SUBSCRIPTION_TYPES: &[&str] = &[
    "workspace.created",
    "workspace.updated",
    "workspace.metadata_updated",
    "workspace.renamed",
    "workspace.moved",
    "workspace.reordered",
    "workspace.closed",
    "workspace.focused",
    "worktree.created",
    "worktree.opened",
    "worktree.removed",
    "tab.created",
    "tab.closed",
    "tab.focused",
    "tab.renamed",
    "tab.moved",
    "pane.created",
    "pane.closed",
    "pane.updated",
    "pane.focused",
    "pane.moved",
    "pane.exited",
    "pane.agent_detected",
    "pane.output_matched",
    "pane.agent_status_changed",
    "pane.scroll_changed",
    "layout.updated",
];

/// One request the mock received, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedRequest {
    pub id: Option<String>,
    pub method: String,
    pub params: Value,
}

/// One scripted event line, emitted `after` the previous scripted line on that stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedEvent {
    pub after: Duration,
    pub line: String,
}

impl ScriptedEvent {
    pub fn after_ms(ms: u64, line: impl Into<String>) -> Self {
        Self {
            after: Duration::from_millis(ms),
            line: line.into(),
        }
    }

    /// Every non-empty line of a `.jsonl` text, each `gap` after the previous one.
    pub fn from_jsonl(text: &str, gap: Duration) -> Vec<Self> {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| Self {
                after: gap,
                line: l.to_string(),
            })
            .collect()
    }

    /// Split a mixed script: dotted `pane.agent_status_changed` lines go to their pane's
    /// status stream, everything else to the lifecycle stream.
    pub fn route(events: Vec<Self>) -> (Vec<Self>, BTreeMap<String, Vec<Self>>) {
        let mut lifecycle = Vec::new();
        let mut status: BTreeMap<String, Vec<Self>> = BTreeMap::new();
        for event in events {
            let parsed: Option<wire::EventLine> = serde_json::from_str(&event.line).ok();
            match parsed {
                Some(raw) if raw.event == PANE_AGENT_STATUS_CHANGED => {
                    let pane = raw
                        .data
                        .get("pane_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    status.entry(pane).or_default().push(event);
                }
                _ => lifecycle.push(event),
            }
        }
        (lifecycle, status)
    }
}

#[derive(Debug, Clone)]
struct Faults {
    delay: Duration,
    stall: bool,
    close_without_response: bool,
    drop_lifecycle_events: HashSet<usize>,
    close_lifecycle_after: Option<usize>,
    refuse_lifecycle: Option<ErrorBody>,
}

#[derive(Debug, Clone)]
struct Config {
    version: String,
    protocol: u32,
    snapshot: Value,
    pane_get: HashMap<String, Value>,
    known_panes: HashSet<String>,
    canned: HashMap<String, Value>,
    lifecycle_script: Vec<ScriptedEvent>,
    status_scripts: HashMap<String, Vec<ScriptedEvent>>,
    faults: Faults,
}

#[derive(Default)]
struct Counters {
    lifecycle_open: usize,
    lifecycle_opened_total: usize,
    lifecycle_closed_total: usize,
    lifecycle_script_played: usize,
    status_open: HashMap<String, usize>,
    status_opened_total: HashMap<String, usize>,
    status_closed_total: HashMap<String, usize>,
    status_refused: HashMap<String, usize>,
    status_script_played: HashMap<String, usize>,
    connections_open: usize,
}

struct Core {
    config: Mutex<Config>,
    requests: Mutex<Vec<RecordedRequest>>,
    subscriptions: Mutex<Vec<Vec<Subscription>>>,
    lifecycle_live: Mutex<Vec<mpsc::UnboundedSender<String>>>,
    status_live: Mutex<HashMap<String, Vec<mpsc::UnboundedSender<String>>>>,
    counters: Mutex<Counters>,
}

/// Builder for both faces of the mock.
#[derive(Debug, Clone)]
pub struct MockHerdrBuilder {
    config: Config,
}

impl Default for MockHerdrBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl MockHerdrBuilder {
    pub fn new() -> Self {
        Self {
            config: Config {
                version: "0.8.2".to_string(),
                protocol: 21,
                snapshot: json!({
                    "type": "session_snapshot",
                    "snapshot": {
                        "version": "0.8.2", "protocol": 21,
                        "workspaces": [], "tabs": [], "panes": [], "layouts": [], "agents": []
                    }
                }),
                pane_get: HashMap::new(),
                known_panes: HashSet::new(),
                canned: HashMap::new(),
                lifecycle_script: Vec::new(),
                status_scripts: HashMap::new(),
                faults: Faults {
                    delay: Duration::ZERO,
                    stall: false,
                    close_without_response: false,
                    drop_lifecycle_events: HashSet::new(),
                    close_lifecycle_after: None,
                    refuse_lifecycle: None,
                },
            },
        }
    }

    /// `ping` answers this protocol.
    pub fn protocol(mut self, protocol: u32) -> Self {
        self.config.protocol = protocol;
        self
    }

    /// `ping` answers this version.
    pub fn version(mut self, version: &str) -> Self {
        self.config.version = version.to_string();
        self
    }

    /// The `session.snapshot` result. Accepts either the full result object
    /// (`{"type":"session_snapshot","snapshot":{...}}`) or a bare snapshot object.
    pub fn snapshot(mut self, snapshot: Value) -> Self {
        self.config.snapshot = wrap_snapshot(snapshot);
        self
    }

    /// Override `pane.get` for one pane (default: served from the snapshot's `panes`).
    pub fn pane_get(mut self, pane_id: &str, pane: Value) -> Self {
        self.config.pane_get.insert(pane_id.to_string(), pane);
        self
    }

    /// A pane that accepts a status subscription even though it is not in the snapshot.
    pub fn known_pane(mut self, pane_id: &str) -> Self {
        self.config.known_panes.insert(pane_id.to_string());
        self
    }

    /// A canned `result` for any other method.
    pub fn canned(mut self, method: &str, result: Value) -> Self {
        self.config.canned.insert(method.to_string(), result);
        self
    }

    /// The script played on every lifecycle subscription connection, from its ack.
    pub fn lifecycle_events(mut self, events: Vec<ScriptedEvent>) -> Self {
        self.config.lifecycle_script = events;
        self
    }

    /// The script played on every status subscription for `pane_id`, from its ack.
    pub fn status_events(mut self, pane_id: &str, events: Vec<ScriptedEvent>) -> Self {
        self.config
            .status_scripts
            .insert(pane_id.to_string(), events);
        self
    }

    /// Fault: refuse the lifecycle subscription with one error line, then close.
    pub fn refuse_lifecycle_subscription(mut self, code: &str, message: &str) -> Self {
        self.config.faults.refuse_lifecycle = Some(ErrorBody {
            code: code.to_string(),
            message: message.to_string(),
        });
        self
    }

    /// Fault: silently drop the `n`th (0-based) event that would be emitted on a lifecycle
    /// stream, scripted or pushed.
    pub fn drop_event(mut self, n: usize) -> Self {
        self.config.faults.drop_lifecycle_events.insert(n);
        self
    }

    /// Fault: close each lifecycle stream after `n` emitted events.
    pub fn close_lifecycle_after(mut self, n: usize) -> Self {
        self.config.faults.close_lifecycle_after = Some(n);
        self
    }

    /// Fault: wait this long before answering any request.
    pub fn delay(mut self, delay: Duration) -> Self {
        self.config.faults.delay = delay;
        self
    }

    /// Fault: accept every connection and never write.
    pub fn stall(mut self) -> Self {
        self.config.faults.stall = true;
        self
    }

    /// Fault: close every connection without a response line.
    pub fn close_without_response(mut self) -> Self {
        self.config.faults.close_without_response = true;
        self
    }

    fn core(self) -> Arc<Core> {
        Arc::new(Core {
            config: Mutex::new(self.config),
            requests: Mutex::new(Vec::new()),
            subscriptions: Mutex::new(Vec::new()),
            lifecycle_live: Mutex::new(Vec::new()),
            status_live: Mutex::new(HashMap::new()),
            counters: Mutex::new(Counters::default()),
        })
    }

    /// The in-memory face.
    pub fn in_memory(self) -> InMemoryHerdr {
        InMemoryHerdr {
            control: MockControl { core: self.core() },
        }
    }

    /// The socket face: bind `path` and serve until `shutdown` or drop.
    pub async fn serve(self, path: &Path) -> std::io::Result<MockHerdr> {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        let core = self.core();
        let accept_core = Arc::clone(&core);
        let accept = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let core = Arc::clone(&accept_core);
                tokio::spawn(async move {
                    let (reader, writer) = stream.into_split();
                    serve_connection(core, reader, writer).await;
                });
            }
        });
        Ok(MockHerdr {
            control: MockControl { core },
            path: path.to_path_buf(),
            accept: Some(accept),
        })
    }
}

fn wrap_snapshot(snapshot: Value) -> Value {
    if snapshot.get("snapshot").is_some() {
        snapshot
    } else {
        json!({ "type": "session_snapshot", "snapshot": snapshot })
    }
}

/// The control surface shared by both faces.
#[derive(Clone)]
pub struct MockControl {
    core: Arc<Core>,
}

impl MockControl {
    /// Every request received so far, in order (subscribe requests included).
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.core.requests.lock().unwrap().clone()
    }

    /// The methods of every request received so far, in order.
    pub fn methods(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.method).collect()
    }

    /// How many requests of `method` were received.
    pub fn count(&self, method: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.method == method)
            .count()
    }

    /// Every subscription set received, in order.
    pub fn subscriptions(&self) -> Vec<Vec<Subscription>> {
        self.core.subscriptions.lock().unwrap().clone()
    }

    /// Forget recorded requests and subscription sets (counters are kept).
    pub fn clear_requests(&self) {
        self.core.requests.lock().unwrap().clear();
        self.core.subscriptions.lock().unwrap().clear();
    }

    /// Replace the snapshot served from now on.
    pub fn set_snapshot(&self, snapshot: Value) {
        self.core.config.lock().unwrap().snapshot = wrap_snapshot(snapshot);
    }

    /// The snapshot currently served (full result object).
    pub fn snapshot(&self) -> Value {
        self.core.config.lock().unwrap().snapshot.clone()
    }

    /// Change the protocol `ping` answers from now on.
    pub fn set_protocol(&self, protocol: u32) {
        self.core.config.lock().unwrap().protocol = protocol;
    }

    /// Push one line onto every open lifecycle stream.
    pub fn push_lifecycle(&self, line: impl Into<String>) {
        let line = line.into();
        let senders = self.core.lifecycle_live.lock().unwrap();
        for tx in senders.iter() {
            let _ = tx.send(line.clone());
        }
    }

    /// Push one line onto every open status stream for `pane_id`.
    pub fn push_status(&self, pane_id: &str, line: impl Into<String>) {
        let line = line.into();
        let senders = self.core.status_live.lock().unwrap();
        if let Some(list) = senders.get(pane_id) {
            for tx in list {
                let _ = tx.send(line.clone());
            }
        }
    }

    /// Server-side close of every open lifecycle stream (simulates herdr's slow-consumer
    /// kill or a restart).
    pub fn close_lifecycle_streams(&self) {
        self.core.lifecycle_live.lock().unwrap().clear();
    }

    /// Server-side close of every open status stream for `pane_id`.
    pub fn close_status_streams(&self, pane_id: &str) {
        self.core.status_live.lock().unwrap().remove(pane_id);
    }

    /// Lifecycle streams currently open.
    pub fn lifecycle_streams_open(&self) -> usize {
        self.core.counters.lock().unwrap().lifecycle_open
    }

    /// Lifecycle streams ever acknowledged.
    pub fn lifecycle_streams_opened_total(&self) -> usize {
        self.core.counters.lock().unwrap().lifecycle_opened_total
    }

    /// Lifecycle streams that have ended (client or server side).
    pub fn lifecycle_streams_closed_total(&self) -> usize {
        self.core.counters.lock().unwrap().lifecycle_closed_total
    }

    /// Status streams currently open for `pane_id`.
    pub fn status_streams_open(&self, pane_id: &str) -> usize {
        self.core
            .counters
            .lock()
            .unwrap()
            .status_open
            .get(pane_id)
            .copied()
            .unwrap_or(0)
    }

    /// Panes with at least one open status stream, sorted.
    pub fn panes_with_open_status_streams(&self) -> Vec<String> {
        let counters = self.core.counters.lock().unwrap();
        let mut panes: Vec<String> = counters
            .status_open
            .iter()
            .filter(|(_, n)| **n > 0)
            .map(|(p, _)| p.clone())
            .collect();
        panes.sort();
        panes
    }

    /// Status streams ever acknowledged for `pane_id`.
    pub fn status_streams_opened_total(&self, pane_id: &str) -> usize {
        self.core
            .counters
            .lock()
            .unwrap()
            .status_opened_total
            .get(pane_id)
            .copied()
            .unwrap_or(0)
    }

    /// Status streams for `pane_id` that have ended.
    pub fn status_streams_closed_total(&self, pane_id: &str) -> usize {
        self.core
            .counters
            .lock()
            .unwrap()
            .status_closed_total
            .get(pane_id)
            .copied()
            .unwrap_or(0)
    }

    /// Status subscriptions for `pane_id` refused with `pane_not_found`.
    pub fn status_subscriptions_refused(&self, pane_id: &str) -> usize {
        self.core
            .counters
            .lock()
            .unwrap()
            .status_refused
            .get(pane_id)
            .copied()
            .unwrap_or(0)
    }

    /// Connections currently open (socket face only).
    pub fn connections_open(&self) -> usize {
        self.core.counters.lock().unwrap().connections_open
    }

    /// Whether the lifecycle script has been played to the end at least once.
    pub fn lifecycle_script_played(&self) -> bool {
        self.core.counters.lock().unwrap().lifecycle_script_played > 0
    }

    /// Whether every configured status script has been played to the end at least once.
    pub fn all_status_scripts_played(&self) -> bool {
        let config = self.core.config.lock().unwrap();
        let counters = self.core.counters.lock().unwrap();
        config.status_scripts.keys().all(|pane| {
            counters
                .status_script_played
                .get(pane)
                .copied()
                .unwrap_or(0)
                > 0
        })
    }

    /// Poll `pred` every millisecond up to `max_ms` (paused clocks advance instantly).
    pub async fn wait_until(
        &self,
        max_ms: u64,
        mut pred: impl FnMut(&MockControl) -> bool,
    ) -> bool {
        for _ in 0..max_ms {
            if pred(self) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        pred(self)
    }
}

// ---------------------------------------------------------------------------------------
// Core dispatch
// ---------------------------------------------------------------------------------------

enum Reply {
    Line(String),
    Stall,
    CloseSilently,
    Refuse(ErrorBody),
    Ack(Feed),
}

enum FeedKind {
    Lifecycle,
    Status(String),
}

struct Feed {
    kind: FeedKind,
    scripted: Vec<ScriptedEvent>,
    live: mpsc::UnboundedReceiver<String>,
    drop: HashSet<usize>,
    close_after: Option<usize>,
}

fn error_line(id: Option<&str>, code: &str, message: &str) -> String {
    json!({ "id": id, "error": { "code": code, "message": message } }).to_string()
}

fn result_line(id: Option<&str>, result: Value) -> String {
    json!({ "id": id, "result": result }).to_string()
}

impl Core {
    fn record(&self, req: &Request) {
        self.requests.lock().unwrap().push(RecordedRequest {
            id: Some(req.id.clone()),
            method: req.method.clone(),
            params: req.params.clone(),
        });
    }

    async fn dispatch_line(&self, raw_line: &str) -> Reply {
        let (delay, stall, close_silently) = {
            let config = self.config.lock().unwrap();
            (
                config.faults.delay,
                config.faults.stall,
                config.faults.close_without_response,
            )
        };
        // Record at receipt (before any fault), so a test can observe an in-flight request.
        if let Ok(req) = serde_json::from_str::<Request>(raw_line)
            && serde_json::from_str::<Value>(raw_line)
                .ok()
                .and_then(|v| v.get("params").cloned())
                .is_some()
        {
            self.record(&req);
        }
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if stall {
            return Reply::Stall;
        }
        if close_silently {
            return Reply::CloseSilently;
        }
        let raw: Value = match serde_json::from_str(raw_line) {
            Ok(v) => v,
            Err(err) => {
                return Reply::Line(error_line(
                    None,
                    "invalid_params",
                    &format!("invalid request line: {err}"),
                ));
            }
        };
        let id = raw.get("id").and_then(Value::as_str).map(str::to_string);
        if raw.get("params").is_none() {
            // herdr requires `params` on every method, including `ping`.
            let method = raw.get("method").and_then(Value::as_str).unwrap_or("?");
            self.requests.lock().unwrap().push(RecordedRequest {
                id: id.clone(),
                method: method.to_string(),
                params: Value::Null,
            });
            return Reply::Line(error_line(
                id.as_deref(),
                "invalid_params",
                "missing field `params`",
            ));
        }
        let req: Request = match serde_json::from_value(raw) {
            Ok(r) => r,
            Err(err) => {
                return Reply::Line(error_line(
                    id.as_deref(),
                    "invalid_params",
                    &format!("invalid request: {err}"),
                ));
            }
        };
        self.dispatch(req)
    }

    fn dispatch(&self, req: Request) -> Reply {
        let id = Some(req.id.as_str());
        let config = self.config.lock().unwrap();
        match req.method.as_str() {
            wire::method::PING => Reply::Line(result_line(
                id,
                json!({
                    "type": "pong",
                    "version": config.version,
                    "protocol": config.protocol,
                    "capabilities": null
                }),
            )),
            wire::method::SESSION_SNAPSHOT => Reply::Line(result_line(id, config.snapshot.clone())),
            wire::method::PANE_GET => {
                let pane_id = req
                    .params
                    .get("pane_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match find_pane(&config, pane_id) {
                    Some(pane) => {
                        Reply::Line(result_line(id, json!({ "type": "pane", "pane": pane })))
                    }
                    None => Reply::Line(error_line(
                        id,
                        wire::error_code::PANE_NOT_FOUND,
                        &format!("pane {pane_id} not found"),
                    )),
                }
            }
            wire::method::PANE_LIST => {
                let panes = config.snapshot["snapshot"]["panes"].clone();
                Reply::Line(result_line(
                    id,
                    json!({ "type": "pane_list", "panes": panes }),
                ))
            }
            wire::method::WORKTREE_LIST => match config.canned.get(wire::method::WORKTREE_LIST) {
                Some(result) => Reply::Line(result_line(id, result.clone())),
                None => Reply::Line(result_line(
                    id,
                    json!({ "type": "worktree_list", "source": null, "worktrees": [] }),
                )),
            },
            wire::method::EVENTS_SUBSCRIBE => {
                let subs: Vec<Subscription> = match req
                    .params
                    .get("subscriptions")
                    .cloned()
                    .map(serde_json::from_value)
                {
                    Some(Ok(subs)) => subs,
                    _ => {
                        return Reply::Refuse(ErrorBody {
                            code: "invalid_params".into(),
                            message: "missing field `subscriptions`".into(),
                        });
                    }
                };
                self.subscriptions.lock().unwrap().push(subs.clone());
                // herdr rejects the whole set on the first unknown `type` (recorded:
                // fixtures/herdr/subscribe_failure.jsonl), before any subscription is built.
                if let Some(unknown) = subs
                    .iter()
                    .find(|s| !KNOWN_SUBSCRIPTION_TYPES.contains(&s.kind.as_str()))
                {
                    return Reply::Refuse(ErrorBody {
                        code: "invalid_request".into(),
                        message: format!(
                            "invalid request: unknown variant `{}`, expected one of {}",
                            unknown.kind,
                            KNOWN_SUBSCRIPTION_TYPES
                                .iter()
                                .map(|t| format!("`{t}`"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    });
                }
                let per_pane = subs.iter().find(|s| s.kind == PANE_AGENT_STATUS_CHANGED);
                if let Some(sub) = per_pane {
                    let Some(pane_id) = sub.pane_id.clone() else {
                        return Reply::Refuse(ErrorBody {
                            code: "invalid_params".into(),
                            message: "pane.agent_status_changed requires pane_id".into(),
                        });
                    };
                    let known = find_pane(&config, &pane_id).is_some()
                        || config.known_panes.contains(&pane_id);
                    if !known {
                        *self
                            .counters
                            .lock()
                            .unwrap()
                            .status_refused
                            .entry(pane_id.clone())
                            .or_default() += 1;
                        return Reply::Refuse(ErrorBody {
                            code: wire::error_code::PANE_NOT_FOUND.into(),
                            message: format!("pane {pane_id} not found"),
                        });
                    }
                    let (tx, rx) = mpsc::unbounded_channel();
                    self.status_live
                        .lock()
                        .unwrap()
                        .entry(pane_id.clone())
                        .or_default()
                        .push(tx);
                    let scripted = config
                        .status_scripts
                        .get(&pane_id)
                        .cloned()
                        .unwrap_or_default();
                    return Reply::Ack(Feed {
                        kind: FeedKind::Status(pane_id),
                        scripted,
                        live: rx,
                        drop: HashSet::new(),
                        close_after: None,
                    });
                }
                if let Some(err) = &config.faults.refuse_lifecycle {
                    return Reply::Refuse(err.clone());
                }
                let (tx, rx) = mpsc::unbounded_channel();
                self.lifecycle_live.lock().unwrap().push(tx);
                Reply::Ack(Feed {
                    kind: FeedKind::Lifecycle,
                    scripted: config.lifecycle_script.clone(),
                    live: rx,
                    drop: config.faults.drop_lifecycle_events.clone(),
                    close_after: config.faults.close_lifecycle_after,
                })
            }
            other => match config.canned.get(other) {
                Some(result) => Reply::Line(result_line(id, result.clone())),
                None => Reply::Line(error_line(
                    id,
                    "invalid_params",
                    &format!("unknown method `{other}`"),
                )),
            },
        }
    }

    fn feed_opened(&self, kind: &FeedKind) {
        let mut c = self.counters.lock().unwrap();
        match kind {
            FeedKind::Lifecycle => {
                c.lifecycle_open += 1;
                c.lifecycle_opened_total += 1;
            }
            FeedKind::Status(pane) => {
                *c.status_open.entry(pane.clone()).or_default() += 1;
                *c.status_opened_total.entry(pane.clone()).or_default() += 1;
            }
        }
    }

    fn feed_closed(&self, kind: &FeedKind) {
        let mut c = self.counters.lock().unwrap();
        match kind {
            FeedKind::Lifecycle => {
                c.lifecycle_open = c.lifecycle_open.saturating_sub(1);
                c.lifecycle_closed_total += 1;
            }
            FeedKind::Status(pane) => {
                let open = c.status_open.entry(pane.clone()).or_default();
                *open = open.saturating_sub(1);
                *c.status_closed_total.entry(pane.clone()).or_default() += 1;
            }
        }
    }

    fn script_played(&self, kind: &FeedKind) {
        let mut c = self.counters.lock().unwrap();
        match kind {
            FeedKind::Lifecycle => c.lifecycle_script_played += 1,
            FeedKind::Status(pane) => {
                *c.status_script_played.entry(pane.clone()).or_default() += 1;
            }
        }
    }

    fn connection_opened(&self) {
        self.counters.lock().unwrap().connections_open += 1;
    }

    fn connection_closed(&self) {
        let mut c = self.counters.lock().unwrap();
        c.connections_open = c.connections_open.saturating_sub(1);
    }
}

fn find_pane(config: &Config, pane_id: &str) -> Option<Value> {
    if let Some(pane) = config.pane_get.get(pane_id) {
        return Some(pane.clone());
    }
    config.snapshot["snapshot"]["panes"]
        .as_array()?
        .iter()
        .find(|p| p.get("pane_id").and_then(Value::as_str) == Some(pane_id))
        .cloned()
}

/// Play a feed onto `writer` until the script and live channel are done, the close-after
/// limit hits, or the client goes away (`reader` hits EOF).
async fn run_feed<W, R>(core: &Core, mut feed: Feed, mut writer: W, mut reader: R)
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    // The caller has already counted this feed as open (so a client sees the counter move
    // the moment its subscribe() returns, even under paused time).
    let mut scripted = std::mem::take(&mut feed.scripted).into_iter();
    let mut pending: Option<(Pin<Box<tokio::time::Sleep>>, String)> = scripted
        .next()
        .map(|e| (Box::pin(tokio::time::sleep(e.after)), e.line));
    if pending.is_none() {
        core.script_played(&feed.kind);
    }
    let mut index = 0usize;
    let mut sent = 0usize;
    let mut sink = [0u8; 256];
    loop {
        let line = tokio::select! {
            read = reader.read(&mut sink) => {
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
            () = async {
                match pending.as_mut() {
                    Some((sleep, _)) => sleep.as_mut().await,
                    None => std::future::pending().await,
                }
            } => {
                let (_, line) = pending.take().expect("pending scripted event");
                pending = scripted
                    .next()
                    .map(|e| (Box::pin(tokio::time::sleep(e.after)), e.line));
                if pending.is_none() {
                    core.script_played(&feed.kind);
                }
                line
            }
            live = feed.live.recv() => {
                match live {
                    Some(line) => line,
                    None => break,
                }
            }
        };
        let this = index;
        index += 1;
        if feed.drop.contains(&this) {
            continue;
        }
        if let Some(limit) = feed.close_after
            && sent >= limit
        {
            break;
        }
        let mut out = line;
        out.push('\n');
        if writer.write_all(out.as_bytes()).await.is_err() || writer.flush().await.is_err() {
            break;
        }
        sent += 1;
        if let Some(limit) = feed.close_after
            && sent >= limit
        {
            break;
        }
    }
    let _ = writer.shutdown().await;
    core.feed_closed(&feed.kind);
}

/// Serve one connection of the socket face: read one line, reply, stream if subscribed.
async fn serve_connection<R, W>(core: Arc<Core>, reader: R, mut writer: W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    core.connection_opened();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let read = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await;
    let ok = matches!(read, Ok(Ok(n)) if n > 0);
    if ok {
        match core.dispatch_line(line.trim_end()).await {
            Reply::Line(out) => {
                let _ = writer.write_all(format!("{out}\n").as_bytes()).await;
                let _ = writer.flush().await;
                let _ = writer.shutdown().await;
            }
            Reply::Stall => {
                // Hold the connection open until the client goes away.
                let mut sink = [0u8; 64];
                loop {
                    match reader.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }
            Reply::CloseSilently => {
                let _ = writer.shutdown().await;
            }
            Reply::Refuse(body) => {
                let out = json!({ "id": null, "error": body }).to_string();
                let _ = writer.write_all(format!("{out}\n").as_bytes()).await;
                let _ = writer.flush().await;
                let _ = writer.shutdown().await;
            }
            Reply::Ack(feed) => {
                let ack = result_line(None, json!({ "type": SUBSCRIPTION_STARTED }));
                core.feed_opened(&feed.kind);
                if writer
                    .write_all(format!("{ack}\n").as_bytes())
                    .await
                    .is_ok()
                    && writer.flush().await.is_ok()
                {
                    run_feed(&core, feed, writer, reader).await;
                } else {
                    core.feed_closed(&feed.kind);
                }
            }
        }
    }
    core.connection_closed();
}

// ---------------------------------------------------------------------------------------
// Socket face
// ---------------------------------------------------------------------------------------

/// The socket face: a server bound to a Unix socket path.
pub struct MockHerdr {
    control: MockControl,
    path: PathBuf,
    accept: Option<JoinHandle<()>>,
}

impl MockHerdr {
    pub fn builder() -> MockHerdrBuilder {
        MockHerdrBuilder::new()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The shared control surface (cloneable, usable from other tasks).
    pub fn control(&self) -> MockControl {
        self.control.clone()
    }

    /// Stop accepting and remove the socket file. Open streams end when their tasks notice.
    pub async fn shutdown(mut self) {
        if let Some(accept) = self.accept.take() {
            accept.abort();
            let _ = accept.await;
        }
        self.control.close_lifecycle_streams();
        self.control.core.status_live.lock().unwrap().clear();
        let _ = std::fs::remove_file(&self.path);
    }
}

impl std::ops::Deref for MockHerdr {
    type Target = MockControl;
    fn deref(&self) -> &MockControl {
        &self.control
    }
}

impl Drop for MockHerdr {
    fn drop(&mut self) {
        if let Some(accept) = self.accept.take() {
            accept.abort();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------------------
// In-memory face
// ---------------------------------------------------------------------------------------

/// The in-memory face: a [`Transport`] over tokio duplex streams. Cloneable; every clone
/// shares the same scripted core and recorded requests.
#[derive(Clone)]
pub struct InMemoryHerdr {
    control: MockControl,
}

/// What [`InMemoryHerdr::raw_call`] yields: the wire outcome of one request line, expressed
/// only in `serde_json` and `tokio::io` types.
///
/// This exists because of the dev-dependency cycle: `lastcall-engine`'s own `--lib` tests are
/// a second compilation of the engine, so the [`Transport`] this crate implements is a
/// different trait from the one those tests see. The engine's client tests wrap this raw
/// surface in a local adapter; every other consumer uses the [`Transport`] impl directly.
pub enum RawOutcome {
    /// One response line; the connection is then closed.
    Line(String),
    /// The server accepted the connection and will never write.
    Stall,
    /// The server closed without a response line.
    ClosedSilently,
    /// One error line instead of the ack; the connection is then closed.
    Refused(String),
    /// The `subscription_started` ack line, then the connection the event lines arrive on.
    Stream {
        ack_line: String,
        conn: Box<dyn AsyncRead + Send + Unpin>,
    },
}

impl InMemoryHerdr {
    pub fn builder() -> MockHerdrBuilder {
        MockHerdrBuilder::new()
    }

    pub fn control(&self) -> MockControl {
        self.control.clone()
    }

    /// Dispatch one request line exactly as the socket face would.
    pub async fn raw_call(&self, request_line: &str) -> RawOutcome {
        let core = Arc::clone(&self.control.core);
        match core.dispatch_line(request_line.trim_end()).await {
            Reply::Line(out) => RawOutcome::Line(out),
            Reply::Stall => RawOutcome::Stall,
            Reply::CloseSilently => RawOutcome::ClosedSilently,
            Reply::Refuse(body) => {
                RawOutcome::Refused(json!({ "id": null, "error": body }).to_string())
            }
            Reply::Ack(feed) => {
                let (client_half, server_half) = tokio::io::duplex(256 * 1024);
                let (server_reader, server_writer) = tokio::io::split(server_half);
                core.feed_opened(&feed.kind);
                tokio::spawn(async move {
                    run_feed(&core, feed, server_writer, server_reader).await;
                });
                RawOutcome::Stream {
                    ack_line: result_line(None, json!({ "type": SUBSCRIPTION_STARTED })),
                    conn: Box::new(client_half),
                }
            }
        }
    }
}

impl std::ops::Deref for InMemoryHerdr {
    type Target = MockControl;
    fn deref(&self) -> &MockControl {
        &self.control
    }
}

impl Transport for InMemoryHerdr {
    fn request(
        &self,
        method: &str,
        params: Value,
    ) -> impl Future<Output = Result<Value, TransportError>> + Send {
        let this = self.clone();
        let line = Request::new("mem", method, params).to_line();
        async move {
            let line = line.map_err(wire::WireError::from)?;
            match this.raw_call(&line).await {
                RawOutcome::Line(out) | RawOutcome::Refused(out) => interpret_response(&out),
                RawOutcome::Stall => std::future::pending().await,
                RawOutcome::ClosedSilently => Err(TransportError::ClosedBeforeResponse),
                RawOutcome::Stream { .. } => Err(TransportError::UnexpectedAck(
                    "ack on a one-shot request".into(),
                )),
            }
        }
    }

    fn subscribe(
        &self,
        subscriptions: Vec<Subscription>,
    ) -> impl Future<Output = Result<EventStream, TransportError>> + Send {
        let this = self.clone();
        let params = serde_json::to_value(wire::EventsSubscribeParams { subscriptions });
        async move {
            let params = params.map_err(wire::WireError::from)?;
            let line = Request::new("mem", wire::method::EVENTS_SUBSCRIBE, params)
                .to_line()
                .map_err(wire::WireError::from)?;
            match this.raw_call(&line).await {
                RawOutcome::Line(out) => {
                    interpret_ack(&out)?;
                    Err(TransportError::UnexpectedAck(
                        "plain line on subscribe".into(),
                    ))
                }
                RawOutcome::Stall => std::future::pending().await,
                RawOutcome::ClosedSilently => Err(TransportError::ClosedBeforeResponse),
                RawOutcome::Refused(out) => {
                    interpret_ack(&out)?;
                    Err(TransportError::UnexpectedAck(
                        "refusal without error".into(),
                    ))
                }
                RawOutcome::Stream { ack_line, conn } => {
                    interpret_ack(&ack_line)?;
                    Ok(EventStream::new(conn))
                }
            }
        }
    }

    fn describe(&self) -> String {
        "in-memory mock herdr".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lastcall_engine::herdr::transport::{StreamEnd, StreamItem};
    use lastcall_engine::herdr::wire::Event;

    fn snapshot_with_pane(pane_id: &str) -> Value {
        json!({
            "version": "0.8.2", "protocol": 21,
            "workspaces": [{"workspace_id": "w", "number": 1, "label": "w", "focused": true,
                            "pane_count": 1, "tab_count": 1, "active_tab_id": "w:t1", "agent_status": "idle"}],
            "tabs": [], "layouts": [], "agents": [],
            "panes": [{"pane_id": pane_id, "terminal_id": "t", "workspace_id": "w", "tab_id": "w:t1",
                       "focused": true, "agent_status": "working", "revision": 0, "agent": "demo"}]
        })
    }

    #[tokio::test(start_paused = true)]
    async fn mock_in_memory_answers_requests_and_records_them() {
        let mock = InMemoryHerdr::builder()
            .snapshot(snapshot_with_pane("w:p1"))
            .protocol(21)
            .in_memory();
        let pong = mock.request("ping", json!({})).await.unwrap();
        assert_eq!(pong["protocol"], 21);
        let snap = mock.request("session.snapshot", json!({})).await.unwrap();
        assert_eq!(snap["type"], "session_snapshot");
        let pane = mock
            .request("pane.get", json!({"pane_id": "w:p1"}))
            .await
            .unwrap();
        assert_eq!(pane["pane"]["pane_id"], "w:p1");
        let err = mock
            .request("pane.get", json!({"pane_id": "zz"}))
            .await
            .unwrap_err();
        assert!(err.is_pane_not_found());
        assert_eq!(
            mock.methods(),
            vec!["ping", "session.snapshot", "pane.get", "pane.get"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn mock_in_memory_streams_script_then_live_then_server_close() {
        let mock = InMemoryHerdr::builder()
            .snapshot(snapshot_with_pane("w:p1"))
            .lifecycle_events(vec![ScriptedEvent::after_ms(
                5,
                r#"{"event":"tab_focused","data":{"type":"tab_focused","tab_id":"w:t1","workspace_id":"w"}}"#,
            )])
            .in_memory();
        let mut stream = mock
            .subscribe(wire::lifecycle_subscriptions())
            .await
            .unwrap();
        assert_eq!(mock.lifecycle_streams_open(), 1);
        let first = stream
            .next(Some(Duration::from_secs(1)))
            .await
            .unwrap()
            .into_event()
            .unwrap();
        assert!(matches!(first, Event::TabFocused { .. }));
        mock.push_lifecycle(
            r#"{"event":"pane_focused","data":{"type":"pane_focused","pane_id":"w:p1","workspace_id":"w"}}"#,
        );
        let second = stream
            .next(Some(Duration::from_secs(1)))
            .await
            .unwrap()
            .into_event()
            .unwrap();
        assert!(matches!(second, Event::PaneFocused { .. }));
        mock.close_lifecycle_streams();
        let end = stream.next(Some(Duration::from_secs(1))).await.unwrap();
        assert_eq!(end, StreamItem::End(StreamEnd::ClosedByPeer));
        assert!(
            mock.wait_until(50, |m| m.lifecycle_streams_open() == 0)
                .await
        );
    }

    #[tokio::test(start_paused = true)]
    async fn mock_in_memory_client_drop_closes_the_feed() {
        let mock = InMemoryHerdr::builder()
            .snapshot(snapshot_with_pane("w:p1"))
            .in_memory();
        let stream = mock
            .subscribe(vec![Subscription::pane_agent_status_changed("w:p1")])
            .await
            .unwrap();
        assert_eq!(mock.status_streams_open("w:p1"), 1);
        drop(stream);
        assert!(
            mock.wait_until(50, |m| m.status_streams_open("w:p1") == 0)
                .await
        );
        assert_eq!(mock.status_streams_closed_total("w:p1"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn mock_in_memory_refuses_unknown_pane_and_missing_pane_id() {
        let mock = InMemoryHerdr::builder()
            .snapshot(snapshot_with_pane("w:p1"))
            .in_memory();
        let err = mock
            .subscribe(vec![Subscription::pane_agent_status_changed("w:p9")])
            .await
            .unwrap_err();
        assert!(err.is_pane_not_found());
        assert_eq!(mock.status_subscriptions_refused("w:p9"), 1);
        assert_eq!(mock.status_streams_open("w:p9"), 0);
        let err = mock
            .subscribe(vec![Subscription::global(PANE_AGENT_STATUS_CHANGED)])
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("invalid_params"));
    }

    #[tokio::test(start_paused = true)]
    async fn mock_in_memory_drop_and_close_after_faults() {
        let mock = InMemoryHerdr::builder()
            .snapshot(snapshot_with_pane("w:p1"))
            .lifecycle_events(vec![
                ScriptedEvent::after_ms(1, r#"{"event":"a","data":{}}"#),
                ScriptedEvent::after_ms(1, r#"{"event":"b","data":{}}"#),
                ScriptedEvent::after_ms(1, r#"{"event":"c","data":{}}"#),
            ])
            .drop_event(1)
            .close_lifecycle_after(2)
            .in_memory();
        let mut stream = mock
            .subscribe(wire::lifecycle_subscriptions())
            .await
            .unwrap();
        let mut names = Vec::new();
        loop {
            match stream.next(Some(Duration::from_secs(1))).await.unwrap() {
                StreamItem::Event(e) => names.push(e.name().to_string()),
                StreamItem::End(end) => {
                    assert_eq!(end, StreamEnd::ClosedByPeer);
                    break;
                }
            }
        }
        assert_eq!(names, vec!["a", "c"]);
    }

    #[test]
    fn mock_route_splits_status_lines_from_lifecycle() {
        let events = ScriptedEvent::from_jsonl(
            r#"{"event":"tab_focused","data":{"type":"tab_focused","tab_id":"w:t1","workspace_id":"w"}}
{"event":"pane.agent_status_changed","data":{"pane_id":"w:p1","workspace_id":"w","agent_status":"done"}}
"#,
            Duration::from_millis(10),
        );
        let (lifecycle, status) = ScriptedEvent::route(events);
        assert_eq!(lifecycle.len(), 1);
        assert_eq!(status.get("w:p1").map(Vec::len), Some(1));
    }
}
