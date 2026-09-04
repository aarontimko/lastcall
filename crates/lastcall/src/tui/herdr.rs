//! The socket-free boundary between the herdr client and the TUI (kickoff deliverables
//! 4, 5, 6, 8).
//!
//! Everything the reducer is allowed to see lives here: [`HerdrUpdate`] (what the loop
//! folds in), [`HerdrView`] (what the reducer keeps), and the pure [`derive`] /
//! [`derive_scope`] that turn a [`Cache`] into them. `app.rs` imports only this module —
//! it never names `lastcall_engine::herdr`, so no `Cache`, `PaneInfo` or `ClientHandle`
//! can reach the reducer (§6.6: a dedicated task owns every socket).
//!
//! The other half of the file is that task's side: [`connect`] (discovery → guard →
//! `Client::spawn`), [`focus`] (`agent.focus`), and [`toast_loop`] — the coalescing
//! `notification.show` sender, the one call site of that method in this crate.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use lastcall_engine::config::{HerdrConfig, HerdrMode, HerdrScope};
use lastcall_engine::env::Env;
use lastcall_engine::herdr::client::{Cache, Client, ClientHandle, ClientOptions, ClientTimings};
use lastcall_engine::herdr::discovery::{self, Discovery};
use lastcall_engine::herdr::transport::{SocketTransport, Transport, socket_answers_ping};
use lastcall_engine::herdr::wire::{
    self, AgentStatus, AgentTarget, NotificationShowParams, NotificationShowResult,
};
use lastcall_engine::herdr::{Compat, HerdrEvent, guard};
use lastcall_engine::roots::Badge;
use tokio::sync::mpsc;

use super::app::RootMeta;

/// The env var herdr sets in every pane it owns; the first half of §6.6 provenance.
pub const WORKSPACE_ID_VAR: &str = "HERDR_WORKSPACE_ID";
/// How long a request to herdr may take before we give up on it.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the first `ping` of discovery may take.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

// ------------------------------------------------------------------------------------------
// The socket-free vocabulary
// ------------------------------------------------------------------------------------------

/// How much a root's agents want the user's eye, in rollup order: `max` over a root's
/// agents is the priority `blocked > done > working > idle > unknown` (deliverable 4), so
/// the derived `Ord` **is** the rule and the test for it is a comparison.
///
/// A TUI-side mirror of the wire's `AgentStatus`: that one carries a `String` in its
/// `Unknown` arm, which the reducer has no use for and which would make every rollup
/// allocate. `Attention::of` folds it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Attention {
    #[default]
    Unknown,
    Idle,
    Working,
    Done,
    Blocked,
}

impl Attention {
    pub fn of(status: &AgentStatus) -> Attention {
        match status {
            AgentStatus::Idle => Attention::Idle,
            AgentStatus::Working => Attention::Working,
            AgentStatus::Blocked => Attention::Blocked,
            AgentStatus::Done => Attention::Done,
            AgentStatus::Unknown(_) => Attention::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Attention::Unknown => "unknown",
            Attention::Idle => "idle",
            Attention::Working => "working",
            Attention::Done => "done",
            Attention::Blocked => "blocked",
        }
    }
}

/// What herdr says about one root's agents, after the §6.6 association walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootAgents {
    /// The max-attention rollup over every agent associated with this root.
    pub status: Attention,
    pub agents: u32,
    /// The public pane id of the max-attention agent (`agent.focus` target).
    pub pane: Option<String>,
    /// That agent's display label, for the `focused <agent> in herdr` status line.
    pub agent: Option<String>,
}

/// The active workspace scope (deliverable 8): which roots the overlay covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// The workspace's label (its id when it has none) — the notice's `<workspace label>`.
    pub label: String,
    pub roots: BTreeSet<PathBuf>,
}

/// What the reducer tells the toast task after a re-derivation or an ack: the display
/// names of the roots whose ready episode just opened, and of those whose episode ended
/// (acked, or the agent moved off `done`) before the coalescing window fired.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToastRequest {
    pub ready: Vec<String>,
    pub dropped: Vec<String>,
}

impl ToastRequest {
    pub fn is_empty(&self) -> bool {
        self.ready.is_empty() && self.dropped.is_empty()
    }
}

/// Which ready episodes one [`HerdrView::apply_roots`] opened and closed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReadyDelta {
    pub opened: Vec<PathBuf>,
    pub closed: Vec<PathBuf>,
}

/// What herdr did with a `notification.show`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastShown {
    pub shown: bool,
    pub reason: String,
}

/// One fold of herdr news into the reducer. Socket-free by construction: no `Cache`, no
/// `PaneInfo`, no handle, nothing that borrows the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HerdrUpdate {
    Connected {
        version: String,
        protocol: u32,
    },
    Standalone {
        reason: String,
    },
    Reconnecting,
    /// The whole association, re-derived; replaces what the view holds.
    Roots(BTreeMap<PathBuf, RootAgents>),
    /// The whole scope, re-derived; `None` when nothing identifies a workspace.
    Scope(Option<Scope>),
    /// A `notification.show` came back. `Ok` carries herdr's verdict.
    Toast(Result<ToastShown, String>),
    /// An `agent.focus` came back. `Ok` carries the label to name in the status line.
    Focused(Result<String, String>),
}

/// The header's herdr badge (deliverable 5).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Link {
    /// `[herdr] mode = "off"`, or no link was ever asked for.
    #[default]
    Off,
    Connected {
        version: String,
    },
    Reconnecting,
    /// No link. `reason` is empty unless `mode = "on"` made the failure visible.
    Standalone {
        reason: String,
    },
}

impl Link {
    /// Whether the status dots may claim anything: only a live link has current data
    /// (§6.6 degradation — on `Reconnecting`/`Standalone` every dot goes neutral).
    pub fn live(&self) -> bool {
        matches!(self, Link::Connected { .. })
    }
}

/// One ready episode: set when the rollup first says `done`, cleared by any non-`done`
/// rollup, and never re-set while it stands (deliverable 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ready {
    pub acked: bool,
}

/// A root's herdr state as the reducer keeps it: the derived facts plus the local,
/// per-process ack episode (ruling 10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootFlag {
    pub status: Attention,
    pub agents: u32,
    pub pane: Option<String>,
    pub agent: Option<String>,
    pub ready: Option<Ready>,
}

impl RootFlag {
    fn of(agents: RootAgents) -> Self {
        Self {
            status: agents.status,
            agents: agents.agents,
            pane: agents.pane,
            agent: agents.agent,
            ready: None,
        }
    }

    /// Whether this root is listed on its herdr state alone (deliverable 5): a ready
    /// episode, acked or not, or a blocked agent. `working`/`idle`/`unknown` only annotate.
    pub fn attention(&self) -> bool {
        self.ready.is_some() || self.status == Attention::Blocked
    }

    /// The label to name in `focused <agent> in herdr`.
    pub fn agent_label(&self) -> String {
        self.agent
            .clone()
            .or_else(|| self.pane.clone())
            .unwrap_or_else(|| "agent".to_owned())
    }
}

/// Everything the reducer keeps about herdr.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HerdrView {
    pub link: Link,
    pub roots: BTreeMap<PathBuf, RootFlag>,
    /// The last derived scope, kept across a reconnect (the roots did not move).
    pub scope: Option<Scope>,
    /// Whether the scope is honoured — `[herdr] scope` at start, `w` from then on.
    pub scoped: bool,
    /// `true` while `[herdr] toast` is on and a link exists: what `Effect::Toast` needs.
    pub toast: bool,
}

impl HerdrView {
    pub fn flag(&self, root: &Path) -> Option<&RootFlag> {
        self.roots.get(root)
    }

    /// The scope actually in force: `None` when none was derived or `w` turned it off.
    pub fn active_scope(&self) -> Option<&Scope> {
        self.scope.as_ref().filter(|_| self.scoped)
    }

    /// Whether `root` survives the active scope. Everything survives when none is active.
    pub fn in_scope(&self, root: &Path) -> bool {
        match self.active_scope() {
            Some(scope) => scope.roots.contains(root),
            None => true,
        }
    }

    /// The dot a root shows, or `None` for no dot: nothing on `idle`, and nothing at all
    /// while the link is not live.
    pub fn dot(&self, root: &Path) -> Option<Dot> {
        if !self.link.live() {
            return None;
        }
        let flag = self.flag(root)?;
        match (flag.ready, flag.status) {
            (Some(Ready { acked }), _) => Some(Dot::Ready { acked }),
            (None, Attention::Blocked) => Some(Dot::Blocked),
            (None, Attention::Working) => Some(Dot::Working),
            (None, Attention::Unknown) => Some(Dot::Unknown),
            (None, Attention::Idle | Attention::Done) => None,
        }
    }

    /// Fold a re-derived association in, preserving every ack episode (deliverable 5):
    /// a `done` rollup opens one, any other rollup closes it.
    pub fn apply_roots(&mut self, derived: BTreeMap<PathBuf, RootAgents>) -> ReadyDelta {
        let mut delta = ReadyDelta::default();
        let mut next: BTreeMap<PathBuf, RootFlag> = BTreeMap::new();
        for (root, agents) in derived {
            let was = self.roots.remove(&root);
            let mut flag = RootFlag::of(agents);
            flag.ready = match (flag.status, was.and_then(|f| f.ready)) {
                // Still the same episode: the ack survives, and no second alert.
                (Attention::Done, Some(ready)) => Some(ready),
                (Attention::Done, None) => {
                    delta.opened.push(root.clone());
                    Some(Ready { acked: false })
                }
                // Any non-`done` rollup ends the episode, so the next `done` re-alerts.
                (_, was_ready) => {
                    if was_ready.is_some() {
                        delta.closed.push(root.clone());
                    }
                    None
                }
            };
            next.insert(root, flag);
        }
        // A root the derivation no longer names lost its agents entirely: episode over.
        for (root, flag) in std::mem::take(&mut self.roots) {
            if flag.ready.is_some() {
                delta.closed.push(root);
            }
        }
        self.roots = next;
        delta
    }

    /// Ack `root`'s flag. `true` when something changed.
    pub fn ack(&mut self, root: &Path) -> bool {
        match self.roots.get_mut(root) {
            Some(flag) => match flag.ready {
                Some(Ready { acked: false }) => {
                    flag.ready = Some(Ready { acked: true });
                    true
                }
                _ => false,
            },
            None => false,
        }
    }
}

/// What the nav paints before a root's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dot {
    /// `⚑`, bright until acked and dim after (ruling 10: the ack is local, herdr still
    /// says `done`).
    Ready { acked: bool },
    /// `●` red.
    Blocked,
    /// `●` yellow.
    Working,
    /// `·` dim.
    Unknown,
}

// ------------------------------------------------------------------------------------------
// The pure derivations
// ------------------------------------------------------------------------------------------

/// §6.6 association: every agent-bearing pane's `foreground_cwd` (fallback `cwd`) walked
/// up to the **deepest** known root; panes that resolve to no root are ignored. Per root,
/// the max-attention rollup and the pane id of the agent that won it.
///
/// Pure: a `Cache` and the app's roots in, a map out. No clock, no I/O, no handle.
pub fn derive(cache: &Cache, roots: &[RootMeta]) -> BTreeMap<PathBuf, RootAgents> {
    let mut out: BTreeMap<PathBuf, RootAgents> = BTreeMap::new();
    for pane in cache.panes.values() {
        let info = &pane.info;
        if !info.is_agent_bearing() {
            continue;
        }
        let Some(cwd) = info.foreground_cwd.as_deref().or(info.cwd.as_deref()) else {
            continue;
        };
        let Some(root) = deepest_root(roots, Path::new(cwd)) else {
            continue;
        };
        // The per-pane status stream is the live truth; the snapshot's own field is the
        // fallback until the first status frame lands (§5.7).
        let status = Attention::of(pane.status.as_ref().unwrap_or(&info.agent_status));
        let entry = out.entry(root.to_path_buf()).or_insert(RootAgents {
            status: Attention::Unknown,
            agents: 0,
            pane: None,
            agent: None,
        });
        entry.agents += 1;
        // `>` and not `>=`: the first agent at the winning level keeps the target, so two
        // agents at the same level never make the jump target flap.
        if entry.pane.is_none() || status > entry.status {
            entry.status = status;
            entry.pane = Some(info.pane_id.clone());
            entry.agent = info.agent_label().map(str::to_owned);
        }
    }
    out
}

/// The longest root that is `path` or a prefix of it (nested repos: the innermost wins).
fn deepest_root<'a>(roots: &'a [RootMeta], path: &Path) -> Option<&'a Path> {
    roots
        .iter()
        .map(|m| m.path.as_path())
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.components().count())
}

/// §6.6 / deliverable 8 scope, provenance first: the workspace named by
/// `HERDR_WORKSPACE_ID` scopes to its `worktree.checkout_path` root plus every root whose
/// badge links it; failing that, to every root containing a pane of that workspace; failing
/// that, to nothing at all (`None` — no scope, no notice).
pub fn derive_scope(cache: &Cache, roots: &[RootMeta], workspace_id: &str) -> Option<Scope> {
    let ws = cache.workspaces.get(workspace_id)?;
    let label = if ws.label.is_empty() {
        ws.workspace_id.clone()
    } else {
        ws.label.clone()
    };
    if let Some(worktree) = &ws.worktree {
        let checkout = Path::new(&worktree.checkout_path);
        if let Some(anchor) = roots.iter().find(|m| m.path == checkout) {
            let mut scoped: BTreeSet<PathBuf> = BTreeSet::new();
            scoped.insert(anchor.path.clone());
            for meta in roots {
                let linked = match &meta.badge {
                    Some(Badge::WorktreeOf(p) | Badge::NestedIn(p)) => *p == anchor.path,
                    None => false,
                };
                if linked {
                    scoped.insert(meta.path.clone());
                }
            }
            return Some(Scope {
                label,
                roots: scoped,
            });
        }
    }
    // No provenance we can place: fall back to pane-cwd containment.
    let scoped: BTreeSet<PathBuf> = cache
        .panes
        .values()
        .filter(|p| p.info.workspace_id == workspace_id)
        .filter_map(|p| p.info.foreground_cwd.as_deref().or(p.info.cwd.as_deref()))
        .filter_map(|cwd| deepest_root(roots, Path::new(cwd)))
        .map(Path::to_path_buf)
        .collect();
    if scoped.is_empty() {
        return None;
    }
    Some(Scope {
        label,
        roots: scoped,
    })
}

// ------------------------------------------------------------------------------------------
// The task side: connect, focus, toast
// ------------------------------------------------------------------------------------------

/// What `commands/tui.rs` resolved from `[herdr]` before the terminal was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrPlan {
    pub mode: HerdrMode,
    pub session: Option<String>,
    /// `[herdr] toast`.
    pub toast: bool,
    /// `[herdr] scope`: the start state of the `w` toggle.
    pub scoped: bool,
    /// `HERDR_WORKSPACE_ID` from the pane we were started in, if any.
    pub workspace_id: Option<String>,
}

impl HerdrPlan {
    pub fn of(config: &HerdrConfig, env: &Env) -> Self {
        Self {
            mode: config.mode,
            session: config.session.clone(),
            toast: config.toast,
            scoped: config.scope == HerdrScope::Workspace,
            workspace_id: env.var(WORKSPACE_ID_VAR).map(str::to_owned),
        }
    }
}

/// A live link: the client task's handle and event stream, plus the transport the loop
/// makes its own one-shot requests on.
pub struct HerdrLink {
    pub handle: ClientHandle,
    pub events: mpsc::Receiver<HerdrEvent>,
    pub transport: SocketTransport,
    pub plan: HerdrPlan,
}

/// Discovery → guard → `Client::spawn`, exactly as `hello_herdr.rs` does it.
///
/// `Err` is the badge to show instead: `Link::Off` for `mode = "off"`, and
/// `Link::Standalone { reason }` otherwise — the reason is empty under `mode = "auto"`
/// (a silent standalone) and the failure text under `mode = "on"` (a visible one).
pub async fn connect(env: &Env, plan: HerdrPlan) -> Result<HerdrLink, Link> {
    if plan.mode == HerdrMode::Off {
        return Err(Link::Off);
    }
    let loud = plan.mode == HerdrMode::On;
    let standalone = |reason: String| {
        Err(Link::Standalone {
            reason: if loud { reason } else { String::new() },
        })
    };
    let path = match discovery::discover(env, plan.session.as_deref(), |p: PathBuf| async move {
        socket_answers_ping(&p, PROBE_TIMEOUT).await
    })
    .await
    {
        Discovery::Socket { path, .. } => path,
        other => return standalone(other.notice().unwrap_or_default()),
    };
    let transport = SocketTransport::new(&path, REQUEST_TIMEOUT);
    match guard::probe(&transport).await {
        Compat::Ok { .. } => {}
        Compat::Mismatch { notice, .. } => return standalone(notice),
        Compat::Absent { reason } => return standalone(reason),
    }
    let (handle, events) = Client::spawn(
        transport.clone(),
        ClientTimings::default(),
        ClientOptions { reconnect: true },
    );
    Ok(HerdrLink {
        handle,
        events,
        transport,
        plan,
    })
}

/// `agent.focus` on `pane_id` — herdr's own public pane id, never a display name
/// (`AgentTarget`; the server resolves the id against the session).
pub async fn focus<T: Transport>(transport: &T, pane_id: &str) -> Result<(), String> {
    let params = serde_json::to_value(AgentTarget {
        target: pane_id.to_owned(),
    })
    .map_err(|e| e.to_string())?;
    transport
        .request(wire::method::AGENT_FOCUS, params)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The coalescing window: herdr's own default `delay_seconds` (1 s) plus the `Finished`
/// lifetime (5 s) plus a second of margin. Raising herdr's `delay_seconds` beyond ≈ 6 s
/// makes our toast lose the race to herdr's own — one retry, no more.
pub const TOAST_DELAY: Duration = Duration::from_secs(7);
/// The one retry a `busy` / `rate_limited` verdict earns.
pub const TOAST_RETRY: Duration = Duration::from_secs(5);
/// Title cap (§5.9).
pub const TOAST_TITLE_MAX: usize = 80;
/// Body cap (§5.9).
pub const TOAST_BODY_MAX: usize = 240;

/// What the reducer tells the toast task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToastMsg {
    /// This root's name went ready; start or extend the window.
    Ready(String),
    /// This root was acked (or its episode ended) before the window fired: drop it.
    Drop(String),
}

/// Title and body for a window, sanitised and capped (§5.9).
pub fn toast_text(names: &[String]) -> (String, String) {
    let title = match names {
        [one] => format!("lastcall: {one} ready for review"),
        many => format!("lastcall: {} repos ready for review", many.len()),
    };
    let body = names
        .iter()
        .map(|n| sanitise(n))
        .collect::<Vec<_>>()
        .join(", ");
    (
        cap(&sanitise(&title), TOAST_TITLE_MAX),
        cap(&body, TOAST_BODY_MAX),
    )
}

/// §5.9: control characters out (a `\n` in a repo name must not forge a second line),
/// runs of whitespace folded to one space.
fn sanitise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars() {
        if ch.is_control() || ch == '\u{7f}' || ch.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(ch);
    }
    out
}

/// Cap at `max` **characters**, with an ellipsis when it bit.
fn cap(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    text.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

/// The one `notification.show` call site in this crate: one request per window, plus at
/// most one retry, and never a third.
async fn show_once<T: Transport>(transport: &T, names: &[String]) -> Result<ToastShown, String> {
    let (title, body) = toast_text(names);
    let params = serde_json::to_value(NotificationShowParams {
        title,
        body: Some(body),
        position: None,
        sound: Some("done".to_owned()),
    })
    .map_err(|e| e.to_string())?;
    let value = transport
        .request(wire::method::NOTIFICATION_SHOW, params)
        .await
        .map_err(|e| e.to_string())?;
    let result: NotificationShowResult =
        serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok(ToastShown {
        shown: result.shown,
        reason: result.reason,
    })
}

/// Whether a refusal earns the single retry (ruling 5).
fn retryable(reason: &str) -> bool {
    matches!(reason, "busy" | "rate_limited")
}

/// The toast task: coalesce every `Ready` inside a [`TOAST_DELAY`] window, drop what was
/// acked meanwhile, then send **one** `notification.show`; retry once after
/// [`TOAST_RETRY`] on `busy`/`rate_limited`, drop on anything else. Ends when `rx` closes.
pub async fn toast_loop<T: Transport>(
    transport: T,
    mut rx: mpsc::UnboundedReceiver<ToastMsg>,
    updates: mpsc::UnboundedSender<HerdrUpdate>,
) {
    let mut pending: Vec<String> = Vec::new();
    let mut deadline: Option<tokio::time::Instant> = None;
    loop {
        // Copied out so the timer future borrows nothing the other arm assigns to.
        let due = deadline;
        let sleep = async {
            match due {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            msg = rx.recv() => match msg {
                None => return,
                Some(ToastMsg::Ready(name)) => {
                    if !pending.contains(&name) {
                        pending.push(name);
                    }
                    deadline = Some(tokio::time::Instant::now() + TOAST_DELAY);
                }
                Some(ToastMsg::Drop(name)) => {
                    pending.retain(|n| *n != name);
                    if pending.is_empty() {
                        deadline = None;
                    }
                }
            },
            _ = sleep => {
                deadline = None;
                let names = std::mem::take(&mut pending);
                if names.is_empty() {
                    continue;
                }
                let first = show_once(&transport, &names).await;
                let outcome = match &first {
                    Ok(shown) if !shown.shown && retryable(&shown.reason) => {
                        tokio::time::sleep(TOAST_RETRY).await;
                        show_once(&transport, &names).await
                    }
                    _ => first,
                };
                match outcome {
                    Ok(shown) if shown.shown => {
                        let _ = updates.send(HerdrUpdate::Toast(Ok(shown)));
                    }
                    Ok(shown) => tracing::debug!(reason = %shown.reason, "toast refused"),
                    Err(e) => tracing::debug!(error = %e, "toast failed"),
                }
            }
        }
    }
}

/// Map one client event to the fold the reducer sees; `None` for an event that only asks
/// for a re-derivation (the caller does that with a fresh snapshot).
pub fn update_of(event: &HerdrEvent) -> Option<HerdrUpdate> {
    match event {
        HerdrEvent::Connected { version, protocol } => Some(HerdrUpdate::Connected {
            version: version.clone(),
            protocol: *protocol,
        }),
        HerdrEvent::Disconnected { .. } => Some(HerdrUpdate::Reconnecting),
        HerdrEvent::Standalone { notice } => Some(HerdrUpdate::Standalone {
            reason: notice.clone(),
        }),
        _ => None,
    }
}

/// Whether `event` means the association (and the scope) must be re-derived from a fresh
/// snapshot: deliverable 4's trigger set.
pub fn rederives(event: &HerdrEvent) -> bool {
    matches!(
        event,
        HerdrEvent::Connected { .. }
            | HerdrEvent::Resync(_)
            | HerdrEvent::AgentStatusChanged { .. }
            | HerdrEvent::PaneAssociation { .. }
    )
}
