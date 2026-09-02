//! The herdr client state machine (docs/spec/00-spec.md §5.3, §5.4, §5.6, §5.7, §6.6).
//!
//! **Events are hints; snapshots are truth (invariant 9).** Every event only ever *schedules*
//! a resync against `session.snapshot` or `pane.get`; no cached state the caller relies on is
//! ever derived solely from having observed an event. The one provisional exception is a
//! `pane_created` for a pane absent from the cache, inserted marked `provisional` so a status
//! subscription can be opened immediately — it emits nothing and is replaced wholesale by the
//! next resync.
//!
//! Topology: one lifecycle subscription connection (the §5.4 set), one
//! `pane.agent_status_changed` connection per agent-bearing pane, one-shot connections for
//! everything else. All of it is written against [`Transport`], so the in-memory mock (unit
//! tests under paused time), the socket mock, and the real socket are interchangeable.
//!
//! Freshness of per-pane status events is decided with a per-pane generation counter: the
//! status task stamps each event with the pane's generation at receipt, and the actor applies
//! it only if the generation has not moved since — a resync (which bumps the generation) always
//! wins a tie. "Newer than the last resync's view" therefore means exactly "received after the
//! last completed resync".

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::guard::{self, Compat};
use super::transport::{StreamEnd, StreamItem, Transport, TransportError};
use super::wire::{
    self, AgentInfo, AgentStatus, Event, PaneAgentStatusChangedEvent, PaneInfo, SessionSnapshot,
    Subscription, WorkspaceInfo,
};

/// Every duration the client uses, injected so tests can shrink (or, under paused time,
/// keep) them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientTimings {
    /// At most one `session.snapshot` (and one `pane.get` per pane) per window, trailing edge.
    pub coalesce: Duration,
    /// The periodic snapshot resync that heals anything the ring buffer dropped.
    pub fallback: Duration,
    /// Per-await timeout on one-shot requests and subscription setup.
    pub request_timeout: Duration,
    /// First reconnect delay; doubles up to `reconnect_max`.
    pub reconnect_initial: Duration,
    pub reconnect_max: Duration,
}

impl Default for ClientTimings {
    fn default() -> Self {
        Self {
            coalesce: Duration::from_millis(500),
            fallback: Duration::from_secs(30),
            request_timeout: Duration::from_secs(5),
            reconnect_initial: Duration::from_millis(250),
            reconnect_max: Duration::from_secs(10),
        }
    }
}

/// Behavioural switches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientOptions {
    /// Re-run the full bootstrap (with backoff) after a disconnect. `hello-herdr --socket`
    /// sets this false so a probe against the mock terminates.
    pub reconnect: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self { reconnect: true }
    }
}

/// What a resync targeted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResyncTarget {
    Snapshot,
    PaneGet(String),
}

/// A worktree event, relayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeChange {
    Created,
    Opened,
    Removed,
}

/// What the client tells the rest of the program.
#[derive(Debug, Clone, PartialEq)]
pub enum HerdrEvent {
    /// Bootstrap succeeded.
    Connected { version: String, protocol: u32 },
    /// The lifecycle connection ended or bootstrap failed at the transport level.
    Disconnected { reason: String },
    /// Protocol mismatch or no server: running standalone.
    Standalone { notice: String },
    /// Every lifecycle event, verbatim (for `hello-herdr`; never truth).
    Lifecycle(Box<Event>),
    /// A resync ran against the named authority.
    Resync(ResyncTarget),
    /// The status of record for a pane changed (deduplicated against last-known state).
    AgentStatusChanged {
        pane_id: String,
        workspace_id: String,
        from: Option<AgentStatus>,
        to: AgentStatus,
        agent: Option<String>,
    },
    /// Emitted for every pane on every resync (cwd fields have no change events, §5.8).
    PaneAssociation {
        pane_id: String,
        workspace_id: String,
        cwd: Option<String>,
        foreground_cwd: Option<String>,
    },
    /// A worktree event from the lifecycle stream.
    WorktreeChanged {
        change: WorktreeChange,
        workspace_id: String,
        path: String,
        branch: Option<String>,
    },
    /// A per-pane status subscription was acknowledged.
    StatusStreamOpened { pane_id: String },
    /// A per-pane status subscription ended (`reason` says why).
    StatusStreamClosed { pane_id: String, reason: String },
}

/// One pane in the cache.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneRecord {
    pub info: PaneInfo,
    /// Inserted from a `pane_created` event, never confirmed by a resync. Emits nothing.
    pub provisional: bool,
    /// The status of record: what the latest snapshot or `pane.get` said, plus per-pane events
    /// newer than that. `None` while provisional.
    pub status: Option<AgentStatus>,
}

/// The cache: the last installed snapshot, keyed by ids (never by `number`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Cache {
    pub version: String,
    pub protocol: u32,
    pub focused_workspace_id: Option<String>,
    pub focused_tab_id: Option<String>,
    pub focused_pane_id: Option<String>,
    pub workspaces: BTreeMap<String, WorkspaceInfo>,
    pub panes: BTreeMap<String, PaneRecord>,
    pub agents: BTreeMap<String, AgentInfo>,
    /// Number of completed resyncs (bootstrap counts as the first).
    pub resyncs: u64,
}

impl Cache {
    fn from_snapshot(snapshot: SessionSnapshot, resyncs: u64) -> Self {
        Self {
            version: snapshot.version,
            protocol: snapshot.protocol,
            focused_workspace_id: snapshot.focused_workspace_id,
            focused_tab_id: snapshot.focused_tab_id,
            focused_pane_id: snapshot.focused_pane_id,
            workspaces: snapshot
                .workspaces
                .into_iter()
                .map(|w| (w.workspace_id.clone(), w))
                .collect(),
            panes: snapshot
                .panes
                .into_iter()
                .map(|p| {
                    let status = Some(p.agent_status.clone());
                    (
                        p.pane_id.clone(),
                        PaneRecord {
                            info: p,
                            provisional: false,
                            status,
                        },
                    )
                })
                .collect(),
            agents: snapshot
                .agents
                .into_iter()
                .map(|a| (a.pane_id.clone(), a))
                .collect(),
            resyncs,
        }
    }

    /// Panes that deserve a status subscription: those with an agent (§5.4), counting the
    /// snapshot's `agents[]` list too.
    fn is_agent_bearing(&self, pane_id: &str) -> bool {
        self.panes
            .get(pane_id)
            .is_some_and(|p| p.info.is_agent_bearing())
            || self.agents.contains_key(pane_id)
    }

    /// The status of record for a pane, if known.
    pub fn status_of(&self, pane_id: &str) -> Option<&AgentStatus> {
        self.panes.get(pane_id).and_then(|p| p.status.as_ref())
    }
}

/// Handle to a spawned client: the snapshot accessor and shutdown.
pub struct ClientHandle {
    shared: Arc<RwLock<Option<Cache>>>,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl ClientHandle {
    /// The last installed cache (a clone), if bootstrap ever succeeded.
    pub fn snapshot(&self) -> Option<Cache> {
        self.shared
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Stop the client: closes every connection and ends the event channel.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let abort = task.abort_handle();
            if tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .is_err()
            {
                tracing::warn!("herdr client did not stop in time; aborting");
                abort.abort();
            }
        }
    }

    /// Whether the client task has finished on its own (e.g. `reconnect: false` after a
    /// disconnect).
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_some_and(JoinHandle::is_finished)
    }
}

impl Drop for ClientHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// The client. Construct with [`Client::spawn`].
pub struct Client;

impl Client {
    /// Spawn the client task on the current runtime. Returns the handle and the event channel.
    pub fn spawn<T: Transport>(
        transport: T,
        timings: ClientTimings,
        options: ClientOptions,
    ) -> (ClientHandle, mpsc::Receiver<HerdrEvent>) {
        let (tx, rx) = mpsc::channel(1024);
        let shared = Arc::new(RwLock::new(None));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let actor = Actor::new(
            Arc::new(transport),
            timings,
            options,
            tx,
            Arc::clone(&shared),
            shutdown_rx,
        );
        let task = tokio::spawn(actor.run());
        (
            ClientHandle {
                shared,
                shutdown: shutdown_tx,
                task: Some(task),
            },
            rx,
        )
    }
}

// ---------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------

/// What the lifecycle reader task sends.
enum LifecycleMsg {
    Event(Box<Event>),
    /// A line that did not parse (garbage, or a known event whose data lacks a field we
    /// require). A hint like any other: schedule a resync, keep streaming (invariant 9).
    Malformed(String),
    End(StreamEnd),
    Error(TransportError),
}

/// What a per-pane status task sends. Every message carries the task's `instance` tag.
enum StatusMsg {
    Opened {
        pane_id: String,
        instance: u64,
    },
    Event {
        pane_id: String,
        instance: u64,
        event: Box<PaneAgentStatusChangedEvent>,
        stamp: u64,
    },
    Closed {
        pane_id: String,
        instance: u64,
        reason: String,
    },
    /// Construction refused by the server (`pane_not_found` or another server code): the pane
    /// is gone, do not retry.
    Refused {
        pane_id: String,
        instance: u64,
        code: String,
    },
    /// Construction failed at the transport level: the reconnect path.
    Failed {
        pane_id: String,
        instance: u64,
        reason: String,
    },
}

impl StatusMsg {
    fn pane_and_instance(&self) -> (&str, u64) {
        match self {
            StatusMsg::Opened {
                pane_id, instance, ..
            }
            | StatusMsg::Event {
                pane_id, instance, ..
            }
            | StatusMsg::Closed {
                pane_id, instance, ..
            }
            | StatusMsg::Refused {
                pane_id, instance, ..
            }
            | StatusMsg::Failed {
                pane_id, instance, ..
            } => (pane_id, *instance),
        }
    }
}

enum SessionEnd {
    Shutdown,
    ConsumerGone,
    Disconnected(String),
}

enum BootstrapError {
    Standalone(String),
    Disconnected(String),
}

type PaneGens = Arc<Mutex<HashMap<String, u64>>>;

struct Actor<T: Transport> {
    transport: Arc<T>,
    timings: ClientTimings,
    options: ClientOptions,
    tx: mpsc::Sender<HerdrEvent>,
    shared: Arc<RwLock<Option<Cache>>>,
    shutdown: watch::Receiver<bool>,
    cache: Option<Cache>,
    resyncs: u64,
    pane_gens: PaneGens,
    status_tx: mpsc::UnboundedSender<StatusMsg>,
    status_rx: mpsc::UnboundedReceiver<StatusMsg>,
    status_tasks: HashMap<String, StatusTask>,
    /// Monotonic per-task tag: a message from an older task instance for the same pane is
    /// ignored (a stale `Closed` after a reconnect must not steal the new task's handle).
    next_status_instance: u64,
    lifecycle_task: Option<JoinHandle<()>>,
    pending_snapshot: Option<Instant>,
    pending_pane_gets: BTreeMap<String, Instant>,
    /// The last `Standalone` notice emitted; identical consecutive notices are not repeated.
    last_notice: Option<String>,
}

/// One per-pane status connection task, tagged with its instance.
struct StatusTask {
    instance: u64,
    task: JoinHandle<()>,
}

impl<T: Transport> Drop for Actor<T> {
    /// The actor owns every connection: if its task is aborted (a dropped `ClientHandle`, or
    /// the shutdown grace period expiring) the reader tasks must not outlive it. A dropped
    /// `JoinHandle` only detaches; abort explicitly.
    fn drop(&mut self) {
        if let Some(task) = self.lifecycle_task.take() {
            task.abort();
        }
        for (_, status) in self.status_tasks.drain() {
            status.task.abort();
        }
    }
}

impl<T: Transport> Actor<T> {
    fn new(
        transport: Arc<T>,
        timings: ClientTimings,
        options: ClientOptions,
        tx: mpsc::Sender<HerdrEvent>,
        shared: Arc<RwLock<Option<Cache>>>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        let (status_tx, status_rx) = mpsc::unbounded_channel();
        Self {
            transport,
            timings,
            options,
            tx,
            shared,
            shutdown,
            cache: None,
            resyncs: 0,
            pane_gens: Arc::new(Mutex::new(HashMap::new())),
            status_tx,
            status_rx,
            status_tasks: HashMap::new(),
            next_status_instance: 1,
            lifecycle_task: None,
            last_notice: None,
            pending_snapshot: None,
            pending_pane_gets: BTreeMap::new(),
        }
    }

    async fn emit(&self, event: HerdrEvent) -> bool {
        self.tx.send(event).await.is_ok()
    }

    fn publish(&self) {
        *self.shared.write().unwrap_or_else(|e| e.into_inner()) = self.cache.clone();
    }

    /// The reconnect loop: bootstrap, run the session, tear down, back off, repeat.
    ///
    /// Backoff is exponential from `reconnect_initial` to `reconnect_max` and resets only after
    /// a session that lasted at least `reconnect_max` (a server that acks and drops keeps
    /// backing off). A standalone verdict (protocol mismatch, unreadable snapshot) is announced
    /// once per distinct notice and re-probed every `reconnect_max`.
    async fn run(mut self) {
        let mut attempt: u32 = 0;
        loop {
            if *self.shutdown.borrow() {
                break;
            }
            let mut standalone = false;
            match self.bootstrap().await {
                Ok(life_rx) => {
                    self.last_notice = None;
                    let started = Instant::now();
                    let end = self.session(life_rx).await;
                    self.teardown_status_tasks();
                    if started.elapsed() >= self.timings.reconnect_max {
                        attempt = 0;
                    }
                    match end {
                        SessionEnd::Shutdown | SessionEnd::ConsumerGone => break,
                        SessionEnd::Disconnected(reason) => {
                            if !self.emit(HerdrEvent::Disconnected { reason }).await {
                                break;
                            }
                        }
                    }
                }
                Err(BootstrapError::Standalone(notice)) => {
                    self.teardown_status_tasks();
                    standalone = true;
                    if self.last_notice.as_deref() != Some(notice.as_str()) {
                        self.last_notice = Some(notice.clone());
                        if !self.emit(HerdrEvent::Standalone { notice }).await {
                            break;
                        }
                    }
                }
                Err(BootstrapError::Disconnected(reason)) => {
                    self.teardown_status_tasks();
                    if !self.emit(HerdrEvent::Disconnected { reason }).await {
                        break;
                    }
                }
            }
            if !self.options.reconnect {
                break;
            }
            let delay = if standalone {
                self.timings.reconnect_max
            } else {
                let delay = self.backoff(attempt);
                attempt = attempt.saturating_add(1);
                delay
            };
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = self.shutdown.changed() => break,
            }
        }
        self.teardown_status_tasks();
    }

    fn backoff(&self, attempt: u32) -> Duration {
        // Never hot-loop: a zero initial delay still yields at least a millisecond.
        let initial = self.timings.reconnect_initial.max(Duration::from_millis(1));
        let scaled = initial.saturating_mul(2u32.saturating_pow(attempt.min(16)));
        scaled.min(self.timings.reconnect_max.max(initial))
    }

    /// §5.3: ping → subscribe → ack → snapshot on a second connection → install → if any event
    /// was buffered during setup, schedule one resync → stream.
    async fn bootstrap(&mut self) -> Result<mpsc::UnboundedReceiver<LifecycleMsg>, BootstrapError> {
        // `Connected` reports what `ping` answered (the identity the guard verified), not a
        // field copied from the snapshot.
        let (version, protocol) = match guard::probe(&*self.transport).await {
            Compat::Ok { version, protocol } => (version, protocol),
            Compat::Mismatch { notice, .. } => return Err(BootstrapError::Standalone(notice)),
            Compat::Absent { reason } => return Err(BootstrapError::Disconnected(reason)),
        };
        let stream = self
            .transport
            .subscribe(wire::lifecycle_subscriptions())
            .await
            .map_err(|err| match err {
                TransportError::SubscribeRefused(body) => BootstrapError::Disconnected(format!(
                    "lifecycle subscription refused: {body} (connection closed by server)"
                )),
                other => {
                    BootstrapError::Disconnected(format!("lifecycle subscription failed: {other}"))
                }
            })?;
        let (life_tx, mut life_rx) = mpsc::unbounded_channel();
        // The reader task starts buffering immediately: nothing pushed during setup is lost.
        // The actor owns the task so teardown closes the lifecycle connection.
        if let Some(previous) = self.lifecycle_task.take() {
            previous.abort();
        }
        self.lifecycle_task = Some(tokio::spawn(async move {
            let mut stream = stream;
            loop {
                match stream.next(None).await {
                    Ok(StreamItem::Event(event)) => {
                        if life_tx.send(LifecycleMsg::Event(event)).is_err() {
                            break;
                        }
                    }
                    Ok(StreamItem::End(end)) => {
                        let _ = life_tx.send(LifecycleMsg::End(end));
                        break;
                    }
                    Err(TransportError::Wire(err)) => {
                        // One bad line is not a dead connection.
                        if life_tx
                            .send(LifecycleMsg::Malformed(err.to_string()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = life_tx.send(LifecycleMsg::Error(err));
                        break;
                    }
                }
            }
        }));
        let snapshot = self.request_snapshot().await.map_err(|err| match err {
            // The server answered but we cannot read it: that is a compatibility problem,
            // not a dead connection. Standalone with a notice, not a reconnect storm.
            TransportError::Wire(err) => BootstrapError::Standalone(format!(
                "herdr answered an unreadable session.snapshot ({err}); running standalone"
            )),
            other => BootstrapError::Disconnected(format!("session.snapshot failed: {other}")),
        })?;
        if !self.emit(HerdrEvent::Connected { version, protocol }).await {
            return Err(BootstrapError::Disconnected("consumer gone".into()));
        }
        if !self.install_snapshot(snapshot).await {
            return Err(BootstrapError::Disconnected("consumer gone".into()));
        }
        // Buffered events are never applied as state (§5.3 step 3): if anything arrived while
        // the snapshot was in flight, one resync covers it.
        let mut buffered = 0usize;
        while let Ok(msg) = life_rx.try_recv() {
            match msg {
                LifecycleMsg::Event(_) | LifecycleMsg::Malformed(_) => buffered += 1,
                LifecycleMsg::End(_) | LifecycleMsg::Error(_) => {
                    return Err(BootstrapError::Disconnected(
                        "lifecycle stream ended during bootstrap".into(),
                    ));
                }
            }
        }
        if buffered > 0 {
            tracing::debug!(
                buffered,
                "events buffered during bootstrap; scheduling one resync"
            );
            self.schedule_snapshot();
        }
        Ok(life_rx)
    }

    async fn request_snapshot(&self) -> Result<SessionSnapshot, TransportError> {
        let result = tokio::time::timeout(
            self.timings.request_timeout,
            self.transport
                .request(wire::method::SESSION_SNAPSHOT, serde_json::json!({})),
        )
        .await
        .map_err(|_| TransportError::Timeout {
            what: "session.snapshot",
            after: self.timings.request_timeout,
        })??;
        let parsed: wire::SnapshotResult =
            serde_json::from_value(result).map_err(wire::WireError::from)?;
        Ok(parsed.snapshot)
    }

    async fn request_pane(&self, pane_id: &str) -> Result<PaneInfo, TransportError> {
        let params = serde_json::to_value(wire::PaneTarget {
            pane_id: pane_id.to_string(),
        })
        .map_err(wire::WireError::from)?;
        let result = tokio::time::timeout(
            self.timings.request_timeout,
            self.transport.request(wire::method::PANE_GET, params),
        )
        .await
        .map_err(|_| TransportError::Timeout {
            what: "pane.get",
            after: self.timings.request_timeout,
        })??;
        let parsed: wire::PaneResult =
            serde_json::from_value(result).map_err(wire::WireError::from)?;
        Ok(parsed.pane)
    }

    /// The streaming loop. Returns when the lifecycle stream ends, on shutdown, or when the
    /// consumer is gone.
    async fn session(&mut self, mut life_rx: mpsc::UnboundedReceiver<LifecycleMsg>) -> SessionEnd {
        let mut fallback = tokio::time::interval_at(
            Instant::now() + self.timings.fallback,
            self.timings.fallback,
        );
        fallback.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let next_deadline = self.next_deadline();
            tokio::select! {
                biased;
                _ = self.shutdown.changed() => return SessionEnd::Shutdown,
                msg = life_rx.recv() => match msg {
                    Some(LifecycleMsg::Event(event)) => {
                        if !self.handle_lifecycle(*event).await {
                            return SessionEnd::ConsumerGone;
                        }
                    }
                    Some(LifecycleMsg::Malformed(what)) => {
                        tracing::warn!(%what, "malformed lifecycle line; scheduling a snapshot resync");
                        self.schedule_snapshot();
                    }
                    Some(LifecycleMsg::End(StreamEnd::ClosedByPeer)) | None => {
                        return SessionEnd::Disconnected("lifecycle stream closed by peer".into());
                    }
                    Some(LifecycleMsg::End(StreamEnd::ClosedAfterError(body))) => {
                        return SessionEnd::Disconnected(format!("lifecycle stream error: {body}"));
                    }
                    Some(LifecycleMsg::Error(err)) => {
                        return SessionEnd::Disconnected(format!("lifecycle stream failed: {err}"));
                    }
                },
                msg = self.status_rx.recv() => {
                    if let Some(msg) = msg {
                        match self.handle_status(msg).await {
                            Ok(true) => {}
                            Ok(false) => return SessionEnd::ConsumerGone,
                            Err(reason) => return SessionEnd::Disconnected(reason),
                        }
                    }
                }
                _ = fallback.tick() => {
                    tracing::debug!("fallback timer: scheduling snapshot resync");
                    self.schedule_snapshot();
                }
                _ = async {
                    match next_deadline {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {
                    match self.run_due_resyncs().await {
                        Ok(true) => {}
                        Ok(false) => return SessionEnd::ConsumerGone,
                        Err(reason) => return SessionEnd::Disconnected(reason),
                    }
                }
            }
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        let pane = self.pending_pane_gets.values().min().copied();
        match (self.pending_snapshot, pane) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Trailing-edge coalescing: the first trigger arms the window; later triggers inside it
    /// merge. At most one snapshot per `coalesce`.
    fn schedule_snapshot(&mut self) {
        if self.pending_snapshot.is_none() {
            self.pending_snapshot = Some(Instant::now() + self.timings.coalesce);
        }
    }

    fn schedule_pane_get(&mut self, pane_id: &str) {
        self.pending_pane_gets
            .entry(pane_id.to_string())
            .or_insert_with(|| Instant::now() + self.timings.coalesce);
    }

    /// Run every resync whose window has elapsed. `Ok(false)` = consumer gone;
    /// `Err` = transport failure (reconnect path).
    async fn run_due_resyncs(&mut self) -> Result<bool, String> {
        let now = Instant::now();
        if self.pending_snapshot.is_some_and(|at| at <= now) {
            self.pending_snapshot = None;
            // A full snapshot covers every pending pane.get.
            self.pending_pane_gets.clear();
            return self.resync_snapshot().await;
        }
        let due: Vec<String> = self
            .pending_pane_gets
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for pane_id in due {
            self.pending_pane_gets.remove(&pane_id);
            if !self.resync_pane(&pane_id).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn resync_snapshot(&mut self) -> Result<bool, String> {
        let snapshot = match self.request_snapshot().await {
            Ok(s) => s,
            // A dead connection, or a snapshot we cannot read: both go through the reconnect
            // path, where bootstrap turns an unreadable snapshot into a standalone notice.
            Err(err) if err.is_transport_failure() || matches!(err, TransportError::Wire(_)) => {
                return Err(format!("session.snapshot failed: {err}"));
            }
            Err(err) => {
                tracing::warn!(%err, "session.snapshot returned an error; will retry on the next trigger");
                self.schedule_snapshot();
                return Ok(true);
            }
        };
        // The marker precedes what the resync changed, so a transcript reads
        // `resync: snapshot` then `[pane] agent done → idle`.
        if !self.emit(HerdrEvent::Resync(ResyncTarget::Snapshot)).await {
            return Ok(false);
        }
        Ok(self.install_snapshot(snapshot).await)
    }

    /// Install a snapshot as truth: replace the cache, emit deduplicated status changes and a
    /// `PaneAssociation` per pane, bump every pane's generation, and reconcile status
    /// subscriptions against `snapshot.panes` (§6.6, F9). Returns false if the consumer is gone.
    async fn install_snapshot(&mut self, snapshot: SessionSnapshot) -> bool {
        self.resyncs += 1;
        let new = Cache::from_snapshot(snapshot, self.resyncs);
        let old = self.cache.take().unwrap_or_default();
        let mut emits = Vec::new();
        for (pane_id, record) in &new.panes {
            let old_status = old.panes.get(pane_id).and_then(|p| p.status.clone());
            let new_status = record
                .status
                .clone()
                .expect("installed records have a status");
            let interesting = new.is_agent_bearing(pane_id) || old_status.is_some();
            if interesting && old_status.as_ref() != Some(&new_status) {
                emits.push(HerdrEvent::AgentStatusChanged {
                    pane_id: pane_id.clone(),
                    workspace_id: record.info.workspace_id.clone(),
                    from: old_status,
                    to: new_status,
                    agent: record.info.agent_label().map(str::to_string),
                });
            }
            emits.push(HerdrEvent::PaneAssociation {
                pane_id: pane_id.clone(),
                workspace_id: record.info.workspace_id.clone(),
                cwd: record.info.cwd.clone(),
                foreground_cwd: record.info.foreground_cwd.clone(),
            });
        }
        {
            let mut gens = self.pane_gens.lock().unwrap_or_else(|e| e.into_inner());
            for pane_id in new.panes.keys() {
                *gens.entry(pane_id.clone()).or_default() += 1;
            }
            gens.retain(|pane_id, _| new.panes.contains_key(pane_id));
        }
        self.cache = Some(new);
        self.publish();
        for event in emits {
            if !self.emit(event).await {
                return false;
            }
        }
        self.reconcile_status_tasks().await
    }

    /// Tear down status connections whose pane is absent from the cache (F9) or no longer
    /// agent-bearing (one connection per *agent-bearing* pane, §5.4); open the missing ones.
    async fn reconcile_status_tasks(&mut self) -> bool {
        let Some(cache) = &self.cache else {
            return true;
        };
        let orphans: Vec<String> = self
            .status_tasks
            .keys()
            .filter(|pane_id| {
                !cache.panes.contains_key(*pane_id) || !cache.is_agent_bearing(pane_id)
            })
            .cloned()
            .collect();
        let wanted: Vec<String> = cache
            .panes
            .keys()
            .filter(|pane_id| {
                cache.is_agent_bearing(pane_id) && !self.status_tasks.contains_key(*pane_id)
            })
            .cloned()
            .collect();
        for pane_id in orphans {
            self.close_status_task(&pane_id);
            let reason = if self
                .cache
                .as_ref()
                .is_some_and(|c| c.panes.contains_key(&pane_id))
            {
                "pane no longer agent-bearing (torn down at resync)"
            } else {
                "pane absent from snapshot (orphan torn down at resync)"
            };
            if !self
                .emit(HerdrEvent::StatusStreamClosed {
                    pane_id,
                    reason: reason.into(),
                })
                .await
            {
                return false;
            }
        }
        for pane_id in wanted {
            self.open_status_task(&pane_id);
        }
        true
    }

    async fn resync_pane(&mut self, pane_id: &str) -> Result<bool, String> {
        match self.request_pane(pane_id).await {
            Ok(pane) => {
                let old_status = self
                    .cache
                    .as_ref()
                    .and_then(|c| c.status_of(pane_id).cloned());
                let new_status = pane.agent_status.clone();
                // The marker first, then what the resync changed.
                let mut emits = vec![HerdrEvent::Resync(ResyncTarget::PaneGet(
                    pane_id.to_string(),
                ))];
                let agent_bearing = pane.is_agent_bearing();
                if (agent_bearing || old_status.is_some())
                    && old_status.as_ref() != Some(&new_status)
                {
                    emits.push(HerdrEvent::AgentStatusChanged {
                        pane_id: pane_id.to_string(),
                        workspace_id: pane.workspace_id.clone(),
                        from: old_status,
                        to: new_status.clone(),
                        agent: pane.agent_label().map(str::to_string),
                    });
                }
                emits.push(HerdrEvent::PaneAssociation {
                    pane_id: pane_id.to_string(),
                    workspace_id: pane.workspace_id.clone(),
                    cwd: pane.cwd.clone(),
                    foreground_cwd: pane.foreground_cwd.clone(),
                });
                if let Some(cache) = &mut self.cache {
                    cache.panes.insert(
                        pane_id.to_string(),
                        PaneRecord {
                            info: pane,
                            provisional: false,
                            status: Some(new_status),
                        },
                    );
                    cache.resyncs += 1;
                }
                self.resyncs += 1;
                *self
                    .pane_gens
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(pane_id.to_string())
                    .or_default() += 1;
                self.publish();
                for event in emits {
                    if !self.emit(event).await {
                        return Ok(false);
                    }
                }
                if agent_bearing && !self.status_tasks.contains_key(pane_id) {
                    self.open_status_task(pane_id);
                }
                Ok(true)
            }
            Err(err) if err.is_pane_not_found() => {
                if !self
                    .emit(HerdrEvent::Resync(ResyncTarget::PaneGet(
                        pane_id.to_string(),
                    )))
                    .await
                {
                    return Ok(false);
                }
                if let Some(cache) = &mut self.cache {
                    cache.panes.remove(pane_id);
                    cache.agents.remove(pane_id);
                }
                self.pane_gens
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(pane_id);
                self.publish();
                if self.status_tasks.contains_key(pane_id) {
                    self.close_status_task(pane_id);
                    if !self
                        .emit(HerdrEvent::StatusStreamClosed {
                            pane_id: pane_id.to_string(),
                            reason: "pane.get: pane_not_found".into(),
                        })
                        .await
                    {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Err(err) if err.is_transport_failure() => Err(format!("pane.get failed: {err}")),
            Err(err) => {
                tracing::warn!(%err, pane_id, "pane.get returned an error; scheduling a snapshot instead");
                self.schedule_snapshot();
                Ok(true)
            }
        }
    }

    /// Rule 2: events schedule work; they do not mutate cached truth. Returns false if the
    /// consumer is gone.
    async fn handle_lifecycle(&mut self, event: Event) -> bool {
        if !self
            .emit(HerdrEvent::Lifecycle(Box::new(event.clone())))
            .await
        {
            return false;
        }
        match event {
            Event::PaneCreated { pane } => {
                let pane_id = pane.pane_id.clone();
                let known = self
                    .cache
                    .as_ref()
                    .is_some_and(|c| c.panes.contains_key(&pane_id));
                if !known {
                    // The only provisional cache write from an event: lets a status
                    // subscription open immediately. Never overwritten by a later event; emits
                    // nothing; replaced wholesale by the next resync.
                    let agent_bearing = pane.is_agent_bearing();
                    if let Some(cache) = &mut self.cache {
                        cache.panes.insert(
                            pane_id.clone(),
                            PaneRecord {
                                info: pane,
                                provisional: true,
                                status: None,
                            },
                        );
                    }
                    self.publish();
                    if agent_bearing {
                        self.open_status_task(&pane_id);
                    }
                }
                self.schedule_snapshot();
            }
            Event::PaneAgentDetected {
                pane_id,
                agent,
                released,
                ..
            } => {
                if !released && agent.is_some() && !self.status_tasks.contains_key(&pane_id) {
                    self.open_status_task(&pane_id);
                }
                self.schedule_snapshot();
            }
            Event::PaneClosed { pane_id, .. } | Event::PaneExited { pane_id, .. } => {
                if self.status_tasks.contains_key(&pane_id) {
                    self.close_status_task(&pane_id);
                    if !self
                        .emit(HerdrEvent::StatusStreamClosed {
                            pane_id: pane_id.clone(),
                            reason: "pane closed/exited".into(),
                        })
                        .await
                    {
                        return false;
                    }
                }
                self.schedule_snapshot();
            }
            Event::PaneFocused { pane_id, .. } => {
                self.schedule_pane_get(&pane_id);
            }
            Event::TabFocused { .. } | Event::WorkspaceFocused { .. } => {
                self.schedule_snapshot();
            }
            Event::WorktreeCreated {
                workspace,
                worktree,
            } => {
                if !self
                    .emit(HerdrEvent::WorktreeChanged {
                        change: WorktreeChange::Created,
                        workspace_id: workspace.workspace_id,
                        path: worktree.path,
                        branch: worktree.branch,
                    })
                    .await
                {
                    return false;
                }
                self.schedule_snapshot();
            }
            Event::WorktreeOpened {
                workspace,
                worktree,
                ..
            } => {
                let change = WorktreeChange::Opened;
                if !self
                    .emit(HerdrEvent::WorktreeChanged {
                        change,
                        workspace_id: workspace.workspace_id,
                        path: worktree.path,
                        branch: worktree.branch,
                    })
                    .await
                {
                    return false;
                }
                self.schedule_snapshot();
            }
            Event::WorktreeRemoved {
                workspace_id,
                worktree,
                ..
            } => {
                if !self
                    .emit(HerdrEvent::WorktreeChanged {
                        change: WorktreeChange::Removed,
                        workspace_id,
                        path: worktree.path,
                        branch: worktree.branch,
                    })
                    .await
                {
                    return false;
                }
                self.schedule_snapshot();
            }
            // `pane_updated` never writes the cache: its agent_status can be older than a
            // completed resync (§5.6). A dotted status event on the *lifecycle* stream is not a
            // per-pane stream event either; both are just hints.
            Event::PaneUpdated { .. }
            | Event::PaneMoved { .. }
            | Event::WorkspaceCreated { .. }
            | Event::WorkspaceUpdated { .. }
            | Event::WorkspaceClosed { .. }
            | Event::PaneAgentStatusChanged(_)
            | Event::Unknown { .. } => {
                self.schedule_snapshot();
            }
        }
        true
    }

    /// Rules 3 and 6 for per-pane status connections. `Ok(false)` = consumer gone;
    /// `Err` = transport failure (reconnect path).
    async fn handle_status(&mut self, msg: StatusMsg) -> Result<bool, String> {
        // Only the current task instance for a pane may speak for it: a message queued by a
        // task that was already replaced (e.g. a `Closed` racing a reconnect on a multi-thread
        // runtime) must not touch the new task's entry.
        {
            let (pane_id, instance) = msg.pane_and_instance();
            let current = self
                .status_tasks
                .get(pane_id)
                .is_some_and(|t| t.instance == instance);
            if !current {
                tracing::debug!(
                    pane_id,
                    instance,
                    "status message from a stale task; ignored"
                );
                return Ok(true);
            }
        }
        match msg {
            StatusMsg::Opened { pane_id, .. } => {
                Ok(self.emit(HerdrEvent::StatusStreamOpened { pane_id }).await)
            }
            StatusMsg::Event {
                pane_id,
                event,
                stamp,
                ..
            } => {
                let current = self
                    .pane_gens
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&pane_id)
                    .copied()
                    .unwrap_or(0);
                if stamp != current {
                    tracing::debug!(
                        pane_id,
                        stamp,
                        current,
                        "status event older than the last resync; dropped"
                    );
                    return Ok(true);
                }
                let Some(cache) = &mut self.cache else {
                    return Ok(true);
                };
                let Some(record) = cache.panes.get_mut(&pane_id) else {
                    return Ok(true);
                };
                if record.provisional {
                    return Ok(true);
                }
                let to = event.agent_status.clone();
                if record.status.as_ref() == Some(&to) {
                    return Ok(true); // deduplicated against last-known state
                }
                let from = record.status.replace(to.clone());
                let workspace_id = if event.workspace_id.is_empty() {
                    record.info.workspace_id.clone()
                } else {
                    event.workspace_id.clone()
                };
                let agent = event
                    .display_agent
                    .clone()
                    .or_else(|| event.agent.clone())
                    .or_else(|| record.info.agent_label().map(str::to_string));
                self.publish();
                Ok(self
                    .emit(HerdrEvent::AgentStatusChanged {
                        pane_id,
                        workspace_id,
                        from,
                        to,
                        agent,
                    })
                    .await)
            }
            StatusMsg::Closed {
                pane_id, reason, ..
            } => {
                // Server-side close of a status stream we still want: drop it and let the next
                // snapshot reopen it if the pane still exists.
                self.close_status_task(&pane_id);
                self.schedule_snapshot();
                Ok(self
                    .emit(HerdrEvent::StatusStreamClosed { pane_id, reason })
                    .await)
            }
            StatusMsg::Refused { pane_id, code, .. } => {
                // Rule 3: pane gone, do not retry. The next snapshot drops the cache entry.
                self.close_status_task(&pane_id);
                self.schedule_snapshot();
                Ok(self
                    .emit(HerdrEvent::StatusStreamClosed {
                        pane_id,
                        reason: format!("subscription refused: {code} (no retry)"),
                    })
                    .await)
            }
            StatusMsg::Failed {
                pane_id, reason, ..
            } => {
                self.close_status_task(&pane_id);
                Err(format!(
                    "status subscription for {pane_id} failed: {reason}"
                ))
            }
        }
    }

    fn open_status_task(&mut self, pane_id: &str) {
        if self.status_tasks.contains_key(pane_id) {
            return;
        }
        let transport = Arc::clone(&self.transport);
        let tx = self.status_tx.clone();
        let gens = Arc::clone(&self.pane_gens);
        let pane = pane_id.to_string();
        let timeout = self.timings.request_timeout;
        let instance = self.next_status_instance;
        self.next_status_instance += 1;
        let task = tokio::spawn(async move {
            let subscribe =
                transport.subscribe(vec![Subscription::pane_agent_status_changed(&pane)]);
            let stream = match tokio::time::timeout(timeout, subscribe).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(TransportError::SubscribeRefused(body))) => {
                    let _ = tx.send(StatusMsg::Refused {
                        pane_id: pane,
                        instance,
                        code: body.code,
                    });
                    return;
                }
                Ok(Err(err)) => {
                    let _ = tx.send(StatusMsg::Failed {
                        pane_id: pane,
                        instance,
                        reason: err.to_string(),
                    });
                    return;
                }
                Err(_) => {
                    let _ = tx.send(StatusMsg::Failed {
                        pane_id: pane,
                        instance,
                        reason: format!(
                            "timed out after {timeout:?} waiting for the subscription ack"
                        ),
                    });
                    return;
                }
            };
            if tx
                .send(StatusMsg::Opened {
                    pane_id: pane.clone(),
                    instance,
                })
                .is_err()
            {
                return;
            }
            let mut stream = stream;
            loop {
                match stream.next(None).await {
                    Ok(StreamItem::Event(event)) => {
                        if let Event::PaneAgentStatusChanged(e) = *event {
                            if e.pane_id != pane {
                                continue;
                            }
                            let stamp = gens
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .get(&pane)
                                .copied()
                                .unwrap_or(0);
                            if tx
                                .send(StatusMsg::Event {
                                    pane_id: pane.clone(),
                                    instance,
                                    event: Box::new(e),
                                    stamp,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    Ok(StreamItem::End(end)) => {
                        let reason = match end {
                            StreamEnd::ClosedByPeer => "closed by peer".to_string(),
                            StreamEnd::ClosedAfterError(body) => {
                                format!("closed after error: {body}")
                            }
                        };
                        let _ = tx.send(StatusMsg::Closed {
                            pane_id: pane,
                            instance,
                            reason,
                        });
                        break;
                    }
                    Err(TransportError::Wire(err)) => {
                        // One bad line is a hint, not a dead stream; the next resync covers it.
                        tracing::warn!(pane, %err, "malformed status line; skipped");
                    }
                    Err(err) => {
                        let _ = tx.send(StatusMsg::Closed {
                            pane_id: pane,
                            instance,
                            reason: format!("stream error: {err}"),
                        });
                        break;
                    }
                }
            }
        });
        self.status_tasks
            .insert(pane_id.to_string(), StatusTask { instance, task });
    }

    fn close_status_task(&mut self, pane_id: &str) {
        if let Some(status) = self.status_tasks.remove(pane_id) {
            // Dropping the task drops its stream, which closes the connection.
            status.task.abort();
        }
    }

    fn teardown_status_tasks(&mut self) {
        if let Some(task) = self.lifecycle_task.take() {
            // Dropping the reader task drops the lifecycle stream: the connection closes.
            task.abort();
        }
        for (_, status) in self.status_tasks.drain() {
            status.task.abort();
        }
        // Messages from aborted tasks may still be queued; their instance tags no longer
        // match any entry, so the handler ignores them.
        while self.status_rx.try_recv().is_ok() {}
    }
}

/// Parse a `session_snapshot` result value (used by `hello-herdr` for its table).
pub fn parse_snapshot_result(value: Value) -> Result<SessionSnapshot, wire::WireError> {
    let parsed: wire::SnapshotResult = serde_json::from_value(value)?;
    Ok(parsed.snapshot)
}

#[cfg(test)]
mod tests {
    use super::super::transport::{EventStream, encode_request, interpret_ack, interpret_response};
    use super::*;
    use lastcall_testkit::mock_herdr::{
        InMemoryHerdr, MockHerdrBuilder, RawOutcome, ScriptedEvent,
    };
    use serde_json::json;

    /// Adapter over the testkit's raw in-memory surface. The engine's `--lib` tests are a
    /// second compilation of this crate, so the testkit's own `impl Transport` targets a
    /// different `Transport` trait than the one these tests see; the raw surface is the seam.
    #[derive(Clone)]
    struct Mem(InMemoryHerdr);

    impl std::ops::Deref for Mem {
        type Target = InMemoryHerdr;
        fn deref(&self) -> &InMemoryHerdr {
            &self.0
        }
    }

    impl Transport for Mem {
        async fn request(&self, method: &str, params: Value) -> Result<Value, TransportError> {
            let (_, line) = encode_request(method, params)?;
            match self.0.raw_call(&line).await {
                RawOutcome::Line(out) | RawOutcome::Refused(out) => interpret_response(&out),
                RawOutcome::Stall => std::future::pending().await,
                RawOutcome::ClosedSilently => Err(TransportError::ClosedBeforeResponse),
                RawOutcome::Stream { .. } => Err(TransportError::UnexpectedAck(
                    "ack on a one-shot request".into(),
                )),
            }
        }

        async fn subscribe(
            &self,
            subscriptions: Vec<Subscription>,
        ) -> Result<EventStream, TransportError> {
            let params = serde_json::to_value(wire::EventsSubscribeParams { subscriptions })
                .map_err(wire::WireError::from)?;
            let (_, line) = encode_request(wire::method::EVENTS_SUBSCRIBE, params)?;
            match self.0.raw_call(&line).await {
                RawOutcome::Line(out) | RawOutcome::Refused(out) => {
                    interpret_ack(&out)?;
                    Err(TransportError::UnexpectedAck(
                        "plain line on subscribe".into(),
                    ))
                }
                RawOutcome::Stall => std::future::pending().await,
                RawOutcome::ClosedSilently => Err(TransportError::ClosedBeforeResponse),
                RawOutcome::Stream { ack_line, conn } => {
                    interpret_ack(&ack_line)?;
                    Ok(EventStream::new(conn))
                }
            }
        }

        fn describe(&self) -> String {
            "in-memory mock herdr (engine test adapter)".to_string()
        }
    }

    const FIXTURES: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../lastcall-testkit/fixtures/herdr"
    );
    // Ids as recorded from the real v0.8.2 binary (fixtures/herdr/recorded).
    const P1: &str = "w1:p1"; // agent-bearing (demo, working), non-active tab t1
    const P2: &str = "w1:p2"; // bare shell, active tab t2
    const P3: &str = "w1:p3"; // not in the snapshot
    const WS: &str = "w1";
    const T1: &str = "w1:t1";

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{FIXTURES}/{name}")).unwrap()
    }

    fn snapshot() -> Value {
        serde_json::from_str(&fixture("snapshot_two_panes.json")).unwrap()
    }

    fn snapshot_with_pane_status(pane_id: &str, status: &str) -> Value {
        let mut v = snapshot();
        for pane in v["snapshot"]["panes"].as_array_mut().unwrap() {
            if pane["pane_id"] == pane_id {
                pane["agent_status"] = json!(status);
            }
        }
        for agent in v["snapshot"]["agents"].as_array_mut().unwrap() {
            if agent["pane_id"] == pane_id {
                agent["agent_status"] = json!(status);
            }
        }
        v
    }

    fn snapshot_without_pane(pane_id: &str) -> Value {
        let mut v = snapshot();
        v["snapshot"]["panes"]
            .as_array_mut()
            .unwrap()
            .retain(|p| p["pane_id"] != pane_id);
        v["snapshot"]["agents"]
            .as_array_mut()
            .unwrap()
            .retain(|a| a["pane_id"] != pane_id);
        v
    }

    fn snapshot_with_extra_pane(pane_id: &str, agent: Option<&str>, status: &str) -> Value {
        let mut v = snapshot();
        v["snapshot"]["panes"].as_array_mut().unwrap().push(json!({
            "pane_id": pane_id, "terminal_id": "term_x", "workspace_id": WS, "tab_id": T1,
            "focused": false, "agent_status": status, "revision": 0, "agent": agent,
            "cwd": "/Users/demo/dev/git/lastcall"
        }));
        v
    }

    fn pane_created_line(pane_id: &str, agent: Option<&str>, status: &str) -> String {
        json!({"event": "pane_created", "data": {"type": "pane_created", "pane": {
            "pane_id": pane_id, "terminal_id": "term_x", "workspace_id": WS, "tab_id": T1,
            "focused": false, "agent_status": status, "revision": 0, "agent": agent
        }}})
        .to_string()
    }

    fn pane_updated_line(pane_id: &str, agent: Option<&str>, status: &str) -> String {
        json!({"event": "pane_updated", "data": {"type": "pane_updated", "pane": {
            "pane_id": pane_id, "terminal_id": "term_x", "workspace_id": WS, "tab_id": T1,
            "focused": false, "agent_status": status, "revision": 7, "agent": agent
        }}})
        .to_string()
    }

    fn pane_ref_line(event: &str, pane_id: &str) -> String {
        json!({"event": event, "data": {"type": event, "pane_id": pane_id, "workspace_id": WS}})
            .to_string()
    }

    fn tab_focused_line() -> String {
        json!({"event": "tab_focused", "data": {"type": "tab_focused", "tab_id": T1, "workspace_id": WS}})
            .to_string()
    }

    fn agent_detected_line(pane_id: &str, agent: &str) -> String {
        json!({"event": "pane_agent_detected", "data": {"type": "pane_agent_detected",
            "pane_id": pane_id, "workspace_id": WS, "agent": agent, "released": false, "final_status": null}})
        .to_string()
    }

    fn status_line(pane_id: &str, status: &str) -> String {
        json!({"event": "pane.agent_status_changed", "data": {
            "pane_id": pane_id, "workspace_id": WS, "agent_status": status, "agent": "demo"}})
        .to_string()
    }

    fn builder() -> MockHerdrBuilder {
        InMemoryHerdr::builder().snapshot(snapshot())
    }

    /// Run until the runtime is quiescent (paused clock: advances by 1 ms only once every
    /// runnable task has run).
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    async fn advance(d: Duration) {
        tokio::time::sleep(d).await;
        settle().await;
    }

    fn drain(rx: &mut mpsc::Receiver<HerdrEvent>) -> Vec<HerdrEvent> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    fn status_changes(events: &[HerdrEvent]) -> Vec<(String, Option<AgentStatus>, AgentStatus)> {
        events
            .iter()
            .filter_map(|e| match e {
                HerdrEvent::AgentStatusChanged {
                    pane_id, from, to, ..
                } => Some((pane_id.clone(), from.clone(), to.clone())),
                _ => None,
            })
            .collect()
    }

    fn resyncs(events: &[HerdrEvent]) -> Vec<ResyncTarget> {
        events
            .iter()
            .filter_map(|e| match e {
                HerdrEvent::Resync(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    fn lifecycle_names(events: &[HerdrEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                HerdrEvent::Lifecycle(ev) => Some(ev.name().to_string()),
                _ => None,
            })
            .collect()
    }

    fn timings() -> ClientTimings {
        ClientTimings::default() // production values; free under paused time
    }

    // ---- fix(review) regression tests (verifier (a) findings 1, 2, 4, 5, 6) ----

    #[tokio::test(start_paused = true)]
    async fn client_dropping_handle_closes_every_connection() {
        // Finding 1: the non-cooperative teardown path (a dropped handle aborts the actor)
        // must not leak the lifecycle or status connections.
        let mock = builder().in_memory();
        let (handle, _rx, _) = boot(&mock).await;
        assert_eq!(mock.lifecycle_streams_open(), 1);
        assert_eq!(mock.status_streams_open(P1), 1);
        drop(handle);
        assert!(
            mock.wait_until(50, |m| m.lifecycle_streams_open() == 0
                && m.status_streams_open(P1) == 0)
                .await,
            "lifecycle {} / status {} still open after drop",
            mock.lifecycle_streams_open(),
            mock.status_streams_open(P1)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn client_malformed_lifecycle_line_is_a_hint_not_a_disconnect() {
        // Finding 2: garbage, or a known event missing a required field, schedules a resync
        // and keeps streaming; it never tears the session down.
        let mock = builder().in_memory();
        let (handle, mut rx, _) = boot(&mock).await;
        mock.push_lifecycle("this is not json");
        mock.push_lifecycle(r#"{"event":"pane_created","data":{"type":"pane_created"}}"#);
        mock.push_lifecycle(r#"{"nothing":"here"}"#);
        settle().await;
        let events = drain(&mut rx);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, HerdrEvent::Disconnected { .. })),
            "{events:?}"
        );
        assert_eq!(mock.lifecycle_streams_opened_total(), 1, "no reconnect");
        assert_eq!(mock.lifecycle_streams_open(), 1);
        // The stream still delivers, and the hints coalesced into one resync.
        mock.push_lifecycle(pane_ref_line("pane_focused", P2));
        advance(timings().coalesce).await;
        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, HerdrEvent::Lifecycle(ev) if ev.pane_id() == Some(P2)))
        );
        assert_eq!(
            mock.count("session.snapshot"),
            2,
            "exactly one resync for the bad lines"
        );
        assert_eq!(mock.count("ping"), 1);
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_unreadable_snapshot_is_standalone_not_a_reconnect_storm() {
        // Finding 2 (bootstrap half) + finding 4: an answer we cannot read degrades to one
        // Standalone notice, re-probed slowly, not a 250 ms reconnect loop.
        let mock = builder()
            .snapshot(json!({ "snapshot": { "panes": "not-an-array" } }))
            .in_memory();
        let (handle, mut rx) =
            Client::spawn(Mem(mock.clone()), timings(), ClientOptions::default());
        advance(Duration::from_secs(60)).await;
        let events = drain(&mut rx);
        let notices: Vec<&HerdrEvent> = events
            .iter()
            .filter(|e| matches!(e, HerdrEvent::Standalone { .. }))
            .collect();
        assert_eq!(notices.len(), 1, "{events:?}");
        assert!(matches!(
            notices[0],
            HerdrEvent::Standalone { notice } if notice.contains("session.snapshot")
        ));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, HerdrEvent::Disconnected { .. })),
            "{events:?}"
        );
        // Re-probed every reconnect_max (10 s): 1 + 60/10 = 7 pings in 60 s, not ~240.
        assert!(mock.count("ping") <= 7, "{} pings", mock.count("ping"));
        assert_eq!(
            mock.lifecycle_streams_open(),
            0,
            "no stream left open between probes"
        );
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_protocol_mismatch_with_reconnect_announces_once_then_connects_when_fixed() {
        // Finding 4: with reconnect on, a mismatch is one notice and a slow re-probe; when the
        // server comes back with a supported protocol the client connects.
        let mock = builder().protocol(22).in_memory();
        let (handle, mut rx) =
            Client::spawn(Mem(mock.clone()), timings(), ClientOptions::default());
        advance(Duration::from_secs(45)).await;
        let events = drain(&mut rx);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, HerdrEvent::Standalone { .. }))
                .count(),
            1,
            "{events:?}"
        );
        let pings = mock.count("ping");
        assert!((2..=6).contains(&pings), "{pings} pings in 45 s");
        mock.set_protocol(21);
        advance(timings().reconnect_max).await;
        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, HerdrEvent::Connected { protocol: 21, .. })),
            "{events:?}"
        );
        assert_eq!(mock.lifecycle_streams_open(), 1);
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_flapping_server_keeps_backing_off() {
        // Finding 5: a server that acks and immediately drops must not reconnect every
        // reconnect_initial forever; the attempt counter resets only after a stable session.
        let mock = builder().in_memory();
        let (handle, _rx, _) = boot(&mock).await;
        let start = Instant::now();
        for _ in 0..4 {
            assert!(
                mock.wait_until(20_000, |m| m.lifecycle_streams_open() > 0)
                    .await
            );
            mock.close_lifecycle_streams();
            settle().await;
        }
        // Four flaps: reconnect delays 250, 500, 1000, 2000 ms → at least 1750 ms elapsed
        // before the fourth reopen; a constant 250 ms would take ~750 ms.
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1750),
            "reconnected too eagerly: {elapsed:?}"
        );
        // Boot + three reconnects were observed open (the fourth reopen is still backing off).
        assert_eq!(mock.lifecycle_streams_opened_total(), 4);
        assert_eq!(mock.count("ping"), 4);
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_released_agent_closes_status_subscription_at_resync() {
        // Finding 6: one connection per *agent-bearing* pane — a pane whose agent went away
        // loses its status connection at the next resync (and regains it on detection).
        let mock = builder().in_memory();
        let (handle, mut rx, _) = boot(&mock).await;
        assert_eq!(mock.status_streams_open(P1), 1);
        let mut released = snapshot();
        for pane in released["snapshot"]["panes"].as_array_mut().unwrap() {
            if pane["pane_id"] == P1 {
                pane.as_object_mut().unwrap().remove("agent");
                pane["agent_status"] = json!("unknown");
            }
        }
        released["snapshot"]["agents"] = json!([]);
        mock.set_snapshot(released);
        mock.push_lifecycle(tab_focused_line());
        advance(timings().coalesce).await;
        assert!(
            mock.wait_until(50, |m| m.status_streams_open(P1) == 0)
                .await
        );
        let events = drain(&mut rx);
        assert!(
            events.iter().any(|e| matches!(
                e,
                HerdrEvent::StatusStreamClosed { pane_id, reason }
                    if pane_id == P1 && reason.contains("no longer agent-bearing")
            )),
            "{events:?}"
        );
        assert!(
            handle.snapshot().unwrap().panes.contains_key(P1),
            "the pane itself stays"
        );
        // The agent comes back: detection reopens the subscription.
        mock.set_snapshot(snapshot());
        mock.push_lifecycle(agent_detected_line(P1, "demo"));
        settle().await;
        assert_eq!(mock.status_streams_open(P1), 1);
        handle.shutdown().await;
    }

    async fn boot(
        mock: &InMemoryHerdr,
    ) -> (ClientHandle, mpsc::Receiver<HerdrEvent>, Vec<HerdrEvent>) {
        let (handle, mut rx) =
            Client::spawn(Mem(mock.clone()), timings(), ClientOptions::default());
        settle().await;
        let events = drain(&mut rx);
        assert!(
            matches!(
                events.first(),
                Some(HerdrEvent::Connected { protocol: 21, .. })
            ),
            "expected Connected first, got {events:?}"
        );
        (handle, rx, events)
    }

    #[tokio::test(start_paused = true)]
    async fn client_bootstrap_subscribes_then_acks_then_snapshots_and_applies_no_buffered_event() {
        // The 10 ms delay on every request guarantees the 0 ms scripted event lands while the
        // snapshot request is in flight, i.e. it is buffered during setup.
        let mock = builder()
            .delay(Duration::from_millis(10))
            .known_pane(P3)
            .lifecycle_events(vec![ScriptedEvent::after_ms(
                0,
                pane_created_line(P3, Some("demo"), "working"),
            )])
            .in_memory();
        let (handle, mut rx) =
            Client::spawn(Mem(mock.clone()), timings(), ClientOptions::default());
        advance(Duration::from_millis(40)).await;
        let events = drain(&mut rx);

        // Order: ping, subscribe (ack), snapshot on a second connection, then the per-pane
        // status subscription for the snapshot's agent-bearing pane.
        let methods = mock.methods();
        assert_eq!(
            &methods[..3],
            ["ping", "events.subscribe", "session.snapshot"]
        );
        let subs = mock.subscriptions();
        assert_eq!(subs[0].len(), 15, "lifecycle set first");
        assert!(matches!(events.first(), Some(HerdrEvent::Connected { .. })));

        // The buffered pane_created was never applied: no provisional entry, no status
        // subscription for P3, no status event for it; one resync scheduled instead.
        let cache = handle.snapshot().unwrap();
        assert!(!cache.panes.contains_key(P3));
        assert_eq!(mock.status_streams_opened_total(P3), 0);
        assert!(status_changes(&events).iter().all(|(p, _, _)| p != P3));
        assert!(
            !lifecycle_names(&events).contains(&"pane_created".to_string()),
            "buffered events are not replayed"
        );
        assert_eq!(mock.count("session.snapshot"), 1);
        advance(timings().coalesce).await;
        assert_eq!(
            mock.count("session.snapshot"),
            2,
            "exactly one resync for the buffered burst"
        );
        assert_eq!(resyncs(&drain(&mut rx)), vec![ResyncTarget::Snapshot]);
        assert_eq!(mock.status_streams_open(P1), 1);
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_parses_both_envelope_shapes_on_one_stream() {
        let script =
            ScriptedEvent::from_jsonl(&fixture("lifecycle_burst.jsonl"), Duration::from_millis(1));
        let expected: Vec<String> = script
            .iter()
            .map(|e| Event::parse_line(&e.line).unwrap().name().to_string())
            .collect();
        let mock = builder().lifecycle_events(script).in_memory();
        let (handle, mut rx, _) = boot(&mock).await;
        advance(Duration::from_millis(50)).await;
        let events = drain(&mut rx);
        assert_eq!(lifecycle_names(&events), expected);
        assert!(expected.contains(&"pane.agent_status_changed".to_string()));
        assert!(expected.contains(&"pane_closed".to_string()));
        // A dotted status event on the lifecycle stream is a hint, not the status of record:
        // the burst's ws_demo2:p1 is not in the snapshot, so no status ever changed.
        assert!(status_changes(&events).is_empty(), "{events:?}");
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_subscribe_failure_closes_connection() {
        let mock = builder()
            .refuse_lifecycle_subscription("invalid_params", "unknown subscription type")
            .in_memory();
        let (handle, mut rx) = Client::spawn(
            Mem(mock.clone()),
            timings(),
            ClientOptions { reconnect: false },
        );
        settle().await;
        let events = drain(&mut rx);
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            HerdrEvent::Disconnected { reason } => {
                assert!(reason.contains("refused"), "{reason}");
                assert!(reason.contains("invalid_params"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            mock.methods(),
            vec!["ping", "events.subscribe"],
            "no snapshot after a refusal"
        );
        assert_eq!(mock.lifecycle_streams_open(), 0);
        assert_eq!(
            mock.lifecycle_streams_opened_total(),
            0,
            "no partial subscription"
        );
        assert!(handle.snapshot().is_none());
        assert!(handle.is_finished(), "reconnect=false: the client exits");
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_opens_status_subscription_on_pane_created() {
        let mock = builder().known_pane(P3).in_memory();
        let (handle, mut rx, _) = boot(&mock).await;
        assert_eq!(mock.status_streams_open(P1), 1, "snapshot's agent pane");
        assert_eq!(mock.status_streams_open(P2), 0, "bare shell gets none");
        mock.push_lifecycle(pane_created_line(P3, Some("demo"), "working"));
        settle().await;
        assert_eq!(mock.status_streams_open(P3), 1);
        let cache = handle.snapshot().unwrap();
        let p3 = cache.panes.get(P3).expect("provisional entry");
        assert!(p3.provisional);
        assert_eq!(p3.status, None);
        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, HerdrEvent::StatusStreamOpened { pane_id } if pane_id == P3))
        );
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_opens_status_subscription_on_pane_agent_detected() {
        let mock = builder().in_memory();
        let (handle, _rx, _) = boot(&mock).await;
        assert_eq!(mock.status_streams_open(P2), 0);
        mock.push_lifecycle(agent_detected_line(P2, "demo"));
        settle().await;
        assert_eq!(mock.status_streams_open(P2), 1);
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_closes_status_subscription_on_pane_closed() {
        let mock = builder().in_memory();
        let (handle, mut rx, _) = boot(&mock).await;
        assert_eq!(mock.status_streams_open(P1), 1);
        mock.push_lifecycle(pane_ref_line("pane_closed", P1));
        settle().await;
        assert!(
            mock.wait_until(50, |m| m.status_streams_open(P1) == 0)
                .await
        );
        assert_eq!(mock.status_streams_closed_total(P1), 1);
        let events = drain(&mut rx);
        assert!(
            events.iter().any(
                |e| matches!(e, HerdrEvent::StatusStreamClosed { pane_id, .. } if pane_id == P1)
            )
        );
        // The cache is untouched until the scheduled resync (events never mutate truth).
        assert!(handle.snapshot().unwrap().panes.contains_key(P1));
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_closes_status_subscription_on_pane_exited() {
        let mock = builder().in_memory();
        let (handle, _rx, _) = boot(&mock).await;
        mock.push_lifecycle(pane_ref_line("pane_exited", P1));
        assert!(
            mock.wait_until(50, |m| m.status_streams_open(P1) == 0)
                .await
        );
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_pane_not_found_at_construction_does_not_retry() {
        // P3 is unknown to the mock: its status subscription is refused with pane_not_found.
        let mock = builder().in_memory();
        let (handle, mut rx, _) = boot(&mock).await;
        mock.push_lifecycle(pane_created_line(P3, Some("demo"), "working"));
        settle().await;
        assert_eq!(mock.status_subscriptions_refused(P3), 1);
        // Through the coalesced resync and a full fallback interval: still exactly one attempt.
        advance(timings().coalesce).await;
        advance(timings().fallback).await;
        assert_eq!(mock.status_subscriptions_refused(P3), 1, "no retry");
        assert_eq!(mock.status_streams_opened_total(P3), 0);
        let events = drain(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e,
            HerdrEvent::StatusStreamClosed { pane_id, reason } if pane_id == P3 && reason.contains("pane_not_found")
        )));
        // The resync replaced the provisional entry wholesale: P3 is gone from the cache.
        assert!(!handle.snapshot().unwrap().panes.contains_key(P3));
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_tears_down_orphan_status_subscription_at_resync() {
        let mock = builder().in_memory();
        let (handle, mut rx, _) = boot(&mock).await;
        assert_eq!(mock.status_streams_open(P1), 1);
        // The pane dies but the pane_closed event was dropped by the ring buffer; herdr keeps
        // the status stream up silently (§6.6). Only the snapshot reveals it.
        mock.set_snapshot(snapshot_without_pane(P1));
        mock.push_lifecycle(tab_focused_line());
        advance(timings().coalesce).await;
        assert!(
            mock.wait_until(50, |m| m.status_streams_open(P1) == 0)
                .await
        );
        assert!(!handle.snapshot().unwrap().panes.contains_key(P1));
        let events = drain(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e,
            HerdrEvent::StatusStreamClosed { pane_id, reason } if pane_id == P1 && reason.contains("orphan")
        )));
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_coalesces_tab_focused_into_one_snapshot() {
        let mock = builder().in_memory();
        let (handle, mut rx, _) = boot(&mock).await;
        assert_eq!(mock.count("session.snapshot"), 1);
        for _ in 0..3 {
            mock.push_lifecycle(tab_focused_line());
            settle().await;
        }
        assert_eq!(
            mock.count("session.snapshot"),
            1,
            "nothing inside the window"
        );
        advance(timings().coalesce).await;
        assert_eq!(
            mock.count("session.snapshot"),
            2,
            "one snapshot at the trailing edge"
        );
        assert_eq!(resyncs(&drain(&mut rx)), vec![ResyncTarget::Snapshot]);
        advance(timings().coalesce).await;
        assert_eq!(
            mock.count("session.snapshot"),
            2,
            "no further snapshot without a trigger"
        );
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_coalesces_pane_focused_into_one_pane_get() {
        let mock = builder().in_memory();
        let (handle, mut rx, _) = boot(&mock).await;
        for _ in 0..3 {
            mock.push_lifecycle(pane_ref_line("pane_focused", P1));
            settle().await;
        }
        assert_eq!(mock.count("pane.get"), 0);
        advance(timings().coalesce).await;
        assert_eq!(mock.count("pane.get"), 1);
        assert_eq!(
            mock.count("session.snapshot"),
            1,
            "a pane focus prefers pane.get"
        );
        assert_eq!(
            resyncs(&drain(&mut rx)),
            vec![ResyncTarget::PaneGet(P1.to_string())]
        );
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_converges_after_dropped_pane_closed_via_fallback_resync() {
        // drop_event(0): the very first lifecycle event (our pane_closed) is silently lost.
        let mock = builder().drop_event(0).in_memory();
        let (handle, mut rx, _) = boot(&mock).await;
        mock.set_snapshot(snapshot_without_pane(P1));
        mock.push_lifecycle(pane_ref_line("pane_closed", P1));
        advance(timings().coalesce).await;
        assert!(
            handle.snapshot().unwrap().panes.contains_key(P1),
            "nothing observed yet"
        );
        assert_eq!(mock.status_streams_open(P1), 1);
        assert_eq!(mock.count("session.snapshot"), 1);
        advance(timings().fallback).await;
        assert_eq!(
            mock.count("session.snapshot"),
            2,
            "the fallback timer healed it"
        );
        assert!(!handle.snapshot().unwrap().panes.contains_key(P1));
        assert!(
            mock.wait_until(50, |m| m.status_streams_open(P1) == 0)
                .await
        );
        assert_eq!(resyncs(&drain(&mut rx)), vec![ResyncTarget::Snapshot]);
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_dedupes_repeated_status_events() {
        let mock = builder().in_memory();
        let (handle, mut rx, boot_events) = boot(&mock).await;
        // The snapshot seeded P1 = working (from None).
        assert_eq!(
            status_changes(&boot_events),
            vec![(P1.to_string(), None, AgentStatus::Working)]
        );
        mock.push_status(P1, status_line(P1, "working"));
        mock.push_status(P1, status_line(P1, "working"));
        settle().await;
        assert!(
            status_changes(&drain(&mut rx)).is_empty(),
            "same status: nothing emitted"
        );
        mock.push_status(P1, status_line(P1, "done"));
        mock.push_status(P1, status_line(P1, "done"));
        settle().await;
        assert_eq!(
            status_changes(&drain(&mut rx)),
            vec![(
                P1.to_string(),
                Some(AgentStatus::Working),
                AgentStatus::Done
            )]
        );
        assert_eq!(
            handle.snapshot().unwrap().status_of(P1),
            Some(&AgentStatus::Done)
        );
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_ignores_stale_pane_updated_after_resync() {
        let mock = builder().in_memory();
        let (handle, mut rx, _) = boot(&mock).await;
        // A resync says P1 is done.
        mock.set_snapshot(snapshot_with_pane_status(P1, "done"));
        mock.push_lifecycle(tab_focused_line());
        advance(timings().coalesce).await;
        assert_eq!(
            status_changes(&drain(&mut rx)),
            vec![(
                P1.to_string(),
                Some(AgentStatus::Working),
                AgentStatus::Done
            )]
        );
        // A stale pane_updated (status working) arrives after that resync: it never writes the
        // cache and emits no status change; it merely schedules a resync, which agrees (done).
        mock.push_lifecycle(pane_updated_line(P1, Some("demo"), "working"));
        settle().await;
        assert_eq!(
            handle.snapshot().unwrap().status_of(P1),
            Some(&AgentStatus::Done)
        );
        advance(timings().coalesce).await;
        let events = drain(&mut rx);
        assert!(status_changes(&events).is_empty(), "{events:?}");
        assert_eq!(resyncs(&events), vec![ResyncTarget::Snapshot]);
        assert_eq!(
            handle.snapshot().unwrap().status_of(P1),
            Some(&AgentStatus::Done)
        );
        let fixture_revision = snapshot()["snapshot"]["panes"][0]["revision"]
            .as_u64()
            .unwrap();
        assert_ne!(
            fixture_revision, 7,
            "the event uses a revision the fixture does not"
        );
        assert_eq!(
            handle.snapshot().unwrap().panes[P1].info.revision,
            fixture_revision,
            "snapshot's PaneInfo, not the event's"
        );
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_provisional_pane_created_emits_nothing_and_is_replaced_by_snapshot() {
        let mock = builder().known_pane(P3).in_memory();
        let (handle, mut rx, _) = boot(&mock).await;
        mock.push_lifecycle(pane_created_line(P3, Some("demo"), "working"));
        settle().await;
        // A status event for the provisional pane: ignored.
        mock.push_status(P3, status_line(P3, "done"));
        settle().await;
        let events = drain(&mut rx);
        assert!(status_changes(&events).is_empty(), "{events:?}");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, HerdrEvent::PaneAssociation { pane_id, .. } if pane_id == P3)),
            "{events:?}"
        );
        let p3 = handle.snapshot().unwrap().panes[P3].clone();
        assert!(p3.provisional);
        assert_eq!(p3.status, None);
        // A later pane_created for the same pane never overwrites the provisional entry.
        mock.push_lifecycle(pane_created_line(P3, Some("other"), "blocked"));
        settle().await;
        assert_eq!(
            handle.snapshot().unwrap().panes[P3].info.agent.as_deref(),
            Some("demo")
        );
        // The next snapshot replaces it wholesale and then the status is of record.
        mock.set_snapshot(snapshot_with_extra_pane(P3, Some("demo"), "done"));
        advance(timings().coalesce).await;
        let p3 = handle.snapshot().unwrap().panes[P3].clone();
        assert!(!p3.provisional);
        assert_eq!(p3.status, Some(AgentStatus::Done));
        let events = drain(&mut rx);
        assert_eq!(
            status_changes(&events),
            vec![(P3.to_string(), None, AgentStatus::Done)]
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, HerdrEvent::PaneAssociation { pane_id, .. } if pane_id == P3))
        );
        assert_eq!(
            mock.status_streams_open(P3),
            1,
            "the subscription opened at pane_created stays"
        );
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_protocol_mismatch_emits_standalone_notice() {
        let mock = builder().protocol(22).in_memory();
        let (handle, mut rx) = Client::spawn(
            Mem(mock.clone()),
            timings(),
            ClientOptions { reconnect: false },
        );
        settle().await;
        let events = drain(&mut rx);
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            HerdrEvent::Standalone { notice } => {
                assert!(notice.contains("protocol 22"), "{notice}");
                assert!(notice.contains("standalone"), "{notice}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(mock.methods(), vec!["ping"], "nothing after a mismatch");
        assert!(handle.snapshot().is_none());
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_reconnects_after_stream_close_with_full_bootstrap() {
        let mock = builder().in_memory();
        let (handle, mut rx, _) = boot(&mock).await;
        assert_eq!(mock.status_streams_open(P1), 1);
        mock.close_lifecycle_streams();
        settle().await;
        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, HerdrEvent::Disconnected { .. })),
            "{events:?}"
        );
        assert!(
            mock.wait_until(50, |m| m.status_streams_open(P1) == 0)
                .await,
            "status subs torn down"
        );
        advance(timings().reconnect_initial).await;
        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, HerdrEvent::Connected { .. })),
            "{events:?}"
        );
        assert_eq!(mock.count("ping"), 2);
        assert_eq!(mock.count("session.snapshot"), 2);
        assert_eq!(mock.lifecycle_streams_opened_total(), 2);
        assert_eq!(mock.status_streams_open(P1), 1);
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_status_event_received_during_resync_is_dropped() {
        let mock = builder().delay(Duration::from_millis(10)).in_memory();
        let (handle, mut rx) =
            Client::spawn(Mem(mock.clone()), timings(), ClientOptions::default());
        advance(Duration::from_millis(50)).await;
        drain(&mut rx);
        mock.push_lifecycle(tab_focused_line());
        // The snapshot request starts at the coalesce edge and sleeps 10 ms in the mock.
        tokio::time::sleep(timings().coalesce + Duration::from_millis(2)).await;
        assert_eq!(mock.count("session.snapshot"), 2, "resync in flight");
        mock.push_status(P1, status_line(P1, "done"));
        advance(Duration::from_millis(20)).await;
        // The resync (which says working) wins the tie; the event was received before it completed.
        assert_eq!(
            handle.snapshot().unwrap().status_of(P1),
            Some(&AgentStatus::Working)
        );
        let events = drain(&mut rx);
        assert!(status_changes(&events).is_empty(), "{events:?}");
        // A status event after the resync completed is applied.
        mock.push_status(P1, status_line(P1, "done"));
        settle().await;
        assert_eq!(
            handle.snapshot().unwrap().status_of(P1),
            Some(&AgentStatus::Done)
        );
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_emits_pane_association_on_every_resync() {
        let mock = builder().in_memory();
        let (handle, mut rx, boot_events) = boot(&mock).await;
        let assoc = |events: &[HerdrEvent]| -> Vec<String> {
            events
                .iter()
                .filter_map(|e| match e {
                    HerdrEvent::PaneAssociation { pane_id, cwd, .. } => {
                        Some(format!("{pane_id}={}", cwd.as_deref().unwrap_or("-")))
                    }
                    _ => None,
                })
                .collect()
        };
        // The recorded snapshot's panes share one cwd; read it rather than hard-code it.
        let cwd = snapshot()["snapshot"]["panes"][0]["cwd"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(cwd.starts_with('/'), "{cwd}");
        assert_eq!(
            assoc(&boot_events),
            vec![format!("{P1}={cwd}"), format!("{P2}={cwd}")]
        );
        mock.push_lifecycle(tab_focused_line());
        advance(timings().coalesce).await;
        assert_eq!(assoc(&drain(&mut rx)).len(), 2);
        mock.push_lifecycle(pane_ref_line("pane_focused", P2));
        advance(timings().coalesce).await;
        assert_eq!(assoc(&drain(&mut rx)), vec![format!("{P2}={cwd}")]);
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_snapshot_resync_emits_done_to_idle_flip() {
        // The silent done→idle flip (§5.7) is caught by the focus-driven resync.
        let mock = builder().in_memory();
        let (handle, mut rx, _) = boot(&mock).await;
        mock.push_status(P1, status_line(P1, "done"));
        settle().await;
        drain(&mut rx);
        mock.set_snapshot(snapshot_with_pane_status(P1, "idle"));
        mock.push_lifecycle(tab_focused_line());
        advance(timings().coalesce).await;
        assert_eq!(
            status_changes(&drain(&mut rx)),
            vec![(P1.to_string(), Some(AgentStatus::Done), AgentStatus::Idle)]
        );
        assert_eq!(
            handle.snapshot().unwrap().status_of(P1),
            Some(&AgentStatus::Idle)
        );
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_pane_get_not_found_removes_pane_and_closes_subscription() {
        let mock = builder().in_memory();
        let (handle, mut rx, _) = boot(&mock).await;
        mock.set_snapshot(snapshot_without_pane(P1));
        mock.push_lifecycle(pane_ref_line("pane_focused", P1));
        advance(timings().coalesce).await;
        assert_eq!(mock.count("pane.get"), 1);
        assert!(!handle.snapshot().unwrap().panes.contains_key(P1));
        assert!(
            mock.wait_until(50, |m| m.status_streams_open(P1) == 0)
                .await
        );
        assert_eq!(
            resyncs(&drain(&mut rx)),
            vec![ResyncTarget::PaneGet(P1.to_string())]
        );
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_worktree_events_are_relayed_and_schedule_a_resync() {
        let mock = builder().in_memory();
        let (handle, mut rx, _) = boot(&mock).await;
        let burst = fixture("lifecycle_burst.jsonl");
        let removed = burst
            .lines()
            .find(|l| l.contains("worktree_removed"))
            .unwrap();
        mock.push_lifecycle(removed);
        settle().await;
        let events = drain(&mut rx);
        assert!(
            events.iter().any(|e| matches!(
                e,
                HerdrEvent::WorktreeChanged { change: WorktreeChange::Removed, path, .. }
                    if path == "/Users/demo/dev/git/lastcall-wt/feature"
            )),
            "{events:?}"
        );
        advance(timings().coalesce).await;
        assert_eq!(mock.count("session.snapshot"), 2);
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn client_shutdown_closes_every_connection() {
        let mock = builder().in_memory();
        let (handle, _rx, _) = boot(&mock).await;
        assert_eq!(mock.lifecycle_streams_open(), 1);
        assert_eq!(mock.status_streams_open(P1), 1);
        handle.shutdown().await;
        assert!(
            mock.wait_until(50, |m| m.lifecycle_streams_open() == 0
                && m.status_streams_open(P1) == 0)
                .await
        );
    }
}
