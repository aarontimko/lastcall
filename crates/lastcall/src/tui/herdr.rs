//! The socket-free boundary between the herdr client and the TUI (kickoff deliverables
//! 4, 5, 6, 8).
//!
//! Everything the reducer is allowed to see lives here: [`HerdrUpdate`] (what the loop
//! folds in), [`HerdrView`] (what the reducer keeps), and the pure [`derive`] /
//! [`derive_scope_with`] that turn a [`Cache`] into them (the convenience wrapper
//! [`derive_scope`] adds exactly one impure step, [`canonical_checkout`], because herdr
//! reports the path the user typed and the engine's roots are resolved). `app.rs` imports
//! only this module —
//! it never names `lastcall_engine::herdr`, so no `Cache`, `PaneInfo` or `ClientHandle`
//! can reach the reducer (§6.6: a dedicated task owns every socket).
//!
//! The other half of the file is that task's side: [`connect`] (discovery → guard →
//! `Client::spawn`), [`focus`] (`agent.focus`), and [`toast_loop`] — the coalescing
//! `notification.show` sender, the one call site of that method in this crate.

use std::borrow::Cow;
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
use lastcall_engine::herdr::{
    BRACKETED_PASTE_END, BRACKETED_PASTE_START, Compat, HerdrEvent, guard,
};
use lastcall_engine::roots::Badge;
use tokio::sync::mpsc;

use super::app::RootMeta;

/// The env var herdr sets in every pane it owns; the first half of §6.6 provenance.
pub const WORKSPACE_ID_VAR: &str = "HERDR_WORKSPACE_ID";
/// The pane we were started in, from herdr's pane environment.
pub const PANE_ID_VAR: &str = "HERDR_PANE_ID";
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

/// What the reducer tells the toast task after a re-derivation or an ack: the roots whose
/// ready episode just opened, and those whose episode ended (acked, or the agent moved off
/// `done`) before the coalescing window fired.
///
/// Both halves are keyed by **path**, never by the display name: two checkouts can share a
/// basename, and a window keyed by name would collapse them into one entry and let an ack
/// of either withdraw both (review (b) F5). The name rides along with an opened episode
/// only so the task can label it — the identity is the path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToastRequest {
    pub ready: Vec<(PathBuf, String)>,
    pub dropped: Vec<PathBuf>,
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
    /// Whether the derived map differs from the one it replaced. `PaneAssociation` is
    /// re-emitted on every resync (every 500 ms coalesce window), so without this every
    /// window ended in a repaint of an identical screen (deliverable 6(b)).
    pub changed: bool,
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
    /// Every agent the export could be staged to, per root ([`agents_for`], across every
    /// workspace). Sent beside `Roots` from the same snapshot: the rollup answers "how is
    /// this root doing?" and this answers "which agent?", and the picker needs the second
    /// (deliverable 10).
    Agents(BTreeMap<PathBuf, Vec<AgentCandidate>>),
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
    /// Every candidate pane per root, across every workspace, from the last derivation.
    pub candidates: BTreeMap<PathBuf, Vec<AgentCandidate>>,
    /// `true` from launch until the first scope verdict, when we were started in a herdr
    /// pane with `[herdr] scope = workspace`. Nothing is listed while it holds: the first
    /// pile can land before the link's first snapshot, and a root listed for 200 ms and
    /// then hidden by the scope is the launch flash the Gate 8 sponsor run recorded. Any
    /// verdict clears it — a derived scope (even `None`), a standalone start, a connect
    /// that failed, a link that dropped before its first snapshot.
    pub scope_pending: bool,
}

impl HerdrView {
    pub fn flag(&self, root: &Path) -> Option<&RootFlag> {
        self.roots.get(root)
    }

    /// The scope actually in force: `None` when none was derived or `w` turned it off.
    pub fn active_scope(&self) -> Option<&Scope> {
        self.scope.as_ref().filter(|_| self.scoped)
    }

    /// The agents a flag on `root` could be staged to, narrowed to the active workspace
    /// when the `w` scope is on — the picker then covers the ground the nav does. The map
    /// itself is derived across every workspace, so turning the scope off widens it again
    /// without waiting for a re-derivation (F15).
    pub fn candidates(&self, root: &Path) -> Vec<AgentCandidate> {
        let all = self.candidates.get(root).cloned().unwrap_or_default();
        match self.active_scope() {
            // `Scope::label` and `AgentCandidate::workspace_label` are the same expression
            // over the same snapshot — the workspace's label, its id when it has none.
            Some(scope) => all
                .into_iter()
                .filter(|c| c.workspace_label == scope.label)
                .collect(),
            None => all,
        }
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
        // Cloned at entry, not compared at the end: the loop below **removes** each root
        // from `self.roots` as it goes and `mem::take`s the leftovers, so by the swap there
        // is nothing left to compare against.
        let before = self.roots.clone();
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
        delta.changed = before != next;
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

/// A workspace's `checkout_path` in the shape a `RootMeta.path` can be compared to:
/// `.`/`..`/`//`/trailing-separator noise removed, then resolved through the filesystem —
/// falling back to the cleaned raw value when the path is not there (a `worktree_removed`
/// the cache still carries, or a checkout on another machine). The one impure step in this
/// half of the module, which is why [`derive_scope_with`] takes it as an argument.
pub fn canonical_checkout(raw: &str) -> PathBuf {
    let cleaned: PathBuf = Path::new(raw).components().collect();
    std::fs::canonicalize(&cleaned).unwrap_or(cleaned)
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
    derive_scope_with(cache, roots, workspace_id, canonical_checkout)
}

/// [`derive_scope`] with the checkout-path resolver injected, so the pure half can be
/// tested without touching a disk. `derive_scope` passes [`canonical_checkout`].
pub fn derive_scope_with(
    cache: &Cache,
    roots: &[RootMeta],
    workspace_id: &str,
    resolve: impl Fn(&str) -> PathBuf,
) -> Option<Scope> {
    let ws = cache.workspaces.get(workspace_id)?;
    let label = if ws.label.is_empty() {
        ws.workspace_id.clone()
    } else {
        ws.label.clone()
    };
    if let Some(worktree) = &ws.worktree {
        // Canonicalised (deliverable 8): herdr reports the path the user typed, while the
        // engine's roots came through `roots::discover`, which resolved every symlink. On
        // macOS that alone is the difference between `/tmp/W/alpha` and
        // `/private/tmp/W/alpha`, and an unresolved compare silently yields no scope.
        let checkout = resolve(&worktree.checkout_path);
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
                roots: with_watched_folders(roots, scoped),
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
        roots: with_watched_folders(roots, scoped),
    })
}

/// A scope plus the watched folders that live inside it. A watched folder carries no badge
/// and no pane need sit in it, so neither arm above can place one, yet a repository's
/// scratch folder is part of the work on that repository: hiding it behind `w` hid the one
/// thing the pane was opened to review. A folder joins when the deepest root around it is
/// in the scope, so one inside an unscoped nested repository stays out. Parents sort before
/// children, which lets a folder inside a folder follow it in a single pass.
fn with_watched_folders(roots: &[RootMeta], mut scoped: BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
    let mut folders: Vec<&RootMeta> = roots
        .iter()
        .filter(|m| m.kind == lastcall_engine::store::RootKind::Draft)
        .collect();
    folders.sort_by(|a, b| a.path.cmp(&b.path));
    for folder in folders {
        let around = roots
            .iter()
            .map(|m| m.path.as_path())
            .filter(|root| *root != folder.path && folder.path.starts_with(root))
            .max_by_key(|root| root.components().count());
        if around.is_some_and(|root| scoped.contains(root)) {
            scoped.insert(folder.path.clone());
        }
    }
    scoped
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
    /// `HERDR_PANE_ID` from the pane we were started in, if any: the one pane whose
    /// `foreground_cwd` is **ours** (see [`own_pane_scrubbed`]).
    pub pane_id: Option<String>,
}

impl HerdrPlan {
    pub fn of(config: &HerdrConfig, env: &Env) -> Self {
        Self {
            mode: config.mode,
            session: config.session.clone(),
            toast: config.toast,
            scoped: config.scope == HerdrScope::Workspace,
            workspace_id: env.var(WORKSPACE_ID_VAR).map(str::to_owned),
            pane_id: env.var(PANE_ID_VAR).map(str::to_owned),
        }
    }
}

/// The cache with our own pane's `foreground_cwd` cleared (its `cwd` stays).
///
/// herdr's `foreground_cwd` is the cwd of the pane's foreground process group, sampled
/// from the process table — and in the pane lastcall runs in, that group is lastcall and
/// every `git` it spawns. During the initial scans herdr reported our pane as being in
/// whichever root a `git` child was running in at that instant, `pane_updated` carried it
/// over, and the pane-containment scope (deliverable 8) narrowed to that root: the Gate 8
/// sponsor run's launch flashed one repo → empty → a different stray repo → all, as the
/// scope went `None` → `{one root}` → `{another}` → `None` behind our own
/// scans. Our own pane's foreground is never evidence about the workspace, so it is
/// dropped before any derivation; the shell's `cwd` is still where the pane lives.
///
/// Borrows when there is nothing to scrub, so the common path copies nothing.
pub fn own_pane_scrubbed<'a>(cache: &'a Cache, own_pane: Option<&str>) -> Cow<'a, Cache> {
    let Some(own) = own_pane else {
        return Cow::Borrowed(cache);
    };
    match cache.panes.get(own) {
        Some(record) if record.info.foreground_cwd.is_some() => {
            let mut scrubbed = cache.clone();
            if let Some(record) = scrubbed.panes.get_mut(own) {
                record.info.foreground_cwd = None;
            }
            Cow::Owned(scrubbed)
        }
        _ => Cow::Borrowed(cache),
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

/// What ends every staged payload, inside the paste markers: the export's last line gets its
/// newline and a blank line follows, so the next flag staged into the same buffer starts on a
/// line of its own and reads as a separate block. Pasted text, not a keystroke — nothing here
/// submits.
pub const STAGE_TAIL: &str = "\n\n";

/// `pane.send_text` on `pane_id`, wrapped in bracketed-paste markers (deliverable 6).
///
/// **Staged, not sent.** The markers make an interactive shell or agent CLI treat the whole
/// payload as pasted text: it lands in the input buffer, however many newlines it holds, and
/// waits for the human to press Enter. That is the whole point of the gesture — lastcall
/// hands the agent the note, the human decides when to submit it.
///
/// The payload ends with [`STAGE_TAIL`] — a newline that closes the export's last line and
/// a blank line after it — **inside** the markers, so it is pasted text like the rest and
/// never the Enter we are avoiding. The sponsor's Gate 7 run (§10 2026-09-05) staged three
/// flags into one pane and found each closing fence running straight into the next flag's
/// header on the same line; a Markdown reader nested flags two and three inside the first
/// code block. The original design left the newline out for fear of submitting, but the
/// export already carries dozens of newlines in its diff: an application that did not honour
/// the paste markers would have submitted at the first of them, so the tail adds no risk.
///
/// The markers are part of `text` because herdr sends the bytes through verbatim, and the
/// *application* on the far side interprets them — the tty line discipline never does (F7).
pub async fn stage<T: Transport>(transport: &T, pane_id: &str, text: &str) -> Result<(), String> {
    let params = serde_json::to_value(wire::PaneSendTextParams {
        pane_id: pane_id.to_owned(),
        text: format!("{BRACKETED_PASTE_START}{text}{STAGE_TAIL}{BRACKETED_PASTE_END}"),
    })
    .map_err(|e| e.to_string())?;
    transport
        .request(wire::method::PANE_SEND_TEXT, params)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// One agent the export could be staged to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCandidate {
    /// herdr's own public pane id — the `pane.send_text` target.
    pub pane_id: String,
    /// The agent's display label, falling back to the pane's label and then its id.
    pub label: String,
    pub status: Attention,
    /// The workspace the pane lives in, by label (its id when it has none). Shown on the
    /// picker row: candidates come from **every** workspace, so the row has to say which.
    pub workspace_label: String,
}

/// Every agent-bearing pane associated with `root`, most-wanting-attention first.
///
/// The same association walk as [`derive`] — `foreground_cwd` (fallback `cwd`) up to the
/// deepest known root — but it keeps *every* pane rather than the one that wins the rollup,
/// because the picker's question is "which agent?" and `derive`'s is "how is this root
/// doing?".
///
/// Across every workspace by default (F15: "exactly one candidate, stage without asking"
/// has to mean one candidate *overall*, or a second agent in another workspace would be
/// silently skipped). `workspace` narrows it to one workspace id, which is what the reducer
/// passes while the `w` scope is on, so the picker covers the same ground the nav does.
///
/// Sorted by attention descending, then label, then pane id: a stable order, and the agent
/// that is blocked on a question is the one at the top.
pub fn agents_for(
    cache: &Cache,
    roots: &[RootMeta],
    root: &Path,
    workspace: Option<&str>,
) -> Vec<AgentCandidate> {
    let mut out: Vec<AgentCandidate> = Vec::new();
    for pane in cache.panes.values() {
        let info = &pane.info;
        if !info.is_agent_bearing() {
            continue;
        }
        if workspace.is_some_and(|w| info.workspace_id != w) {
            continue;
        }
        let Some(cwd) = info.foreground_cwd.as_deref().or(info.cwd.as_deref()) else {
            continue;
        };
        if deepest_root(roots, Path::new(cwd)) != Some(root) {
            continue;
        }
        out.push(AgentCandidate {
            pane_id: info.pane_id.clone(),
            label: info
                .agent_label()
                .or(info.label.as_deref())
                .unwrap_or(&info.pane_id)
                .to_owned(),
            status: Attention::of(pane.status.as_ref().unwrap_or(&info.agent_status)),
            workspace_label: cache
                .workspaces
                .get(&info.workspace_id)
                .map(|w| w.label.clone())
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| info.workspace_id.clone()),
        });
    }
    out.sort_by(|a, b| {
        b.status
            .cmp(&a.status)
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.pane_id.cmp(&b.pane_id))
    });
    out
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

/// What the reducer tells the toast task, one root at a time. Keyed by path (review (b)
/// F5); `name` is what the window should call this root when it fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToastMsg {
    /// This root went ready; start or extend the window.
    Ready { root: PathBuf, name: String },
    /// This root was acked (or its episode ended) before the window fired: drop it.
    Drop(PathBuf),
}

/// The names one window shows, formatted at fire time from the pending roots (review (b)
/// F5). A name two pending roots share is qualified by its parent directory, so `/A/proj`
/// and `/B/proj` read as `A/proj, B/proj` rather than twice the same word.
pub fn toast_names(pending: &[(PathBuf, String)]) -> Vec<String> {
    let shared = |name: &str| pending.iter().filter(|(_, n)| n == name).count() > 1;
    pending
        .iter()
        .map(
            |(root, name)| match root.parent().and_then(Path::file_name) {
                Some(parent) if shared(name) => format!("{}/{name}", parent.to_string_lossy()),
                _ => name.clone(),
            },
        )
        .collect()
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

/// One window's `notification.show`, with the verdict folded in: `Some(held)` when the
/// refusal earns the single retry (ruling 5), `None` when there is nothing more to do.
async fn show_window<T: Transport>(
    transport: &T,
    pending: &[(PathBuf, String)],
    updates: &mpsc::UnboundedSender<HerdrUpdate>,
) -> Option<Vec<(PathBuf, String)>> {
    match show_once(transport, &toast_names(pending)).await {
        Ok(shown) if shown.shown => {
            let _ = updates.send(HerdrUpdate::Toast(Ok(shown)));
            None
        }
        Ok(shown) if retryable(&shown.reason) => {
            tracing::debug!(reason = %shown.reason, "toast refused; one retry");
            Some(pending.to_vec())
        }
        Ok(shown) => {
            tracing::debug!(reason = %shown.reason, "toast refused");
            None
        }
        Err(e) => {
            tracing::debug!(error = %e, "toast failed");
            None
        }
    }
}

/// A deadline that may not exist: `None` never resolves, so the arm stays parked.
async fn due_at(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The toast task: coalesce every `Ready` inside a [`TOAST_DELAY`] window, drop what was
/// acked meanwhile, then send **one** `notification.show`; retry once after
/// [`TOAST_RETRY`] on `busy`/`rate_limited`, drop on anything else. Ends when `rx` closes.
///
/// The retry is **timer state**, not a `sleep` inside the arm (review (b) F6): while it
/// waits, an ack still withdraws its root, and a root that goes ready still starts its own
/// 7 s window from the moment it arrived rather than from the end of the wait.
pub async fn toast_loop<T: Transport>(
    transport: T,
    mut rx: mpsc::UnboundedReceiver<ToastMsg>,
    updates: mpsc::UnboundedSender<HerdrUpdate>,
) {
    // Arrival order, one entry per path.
    let mut pending: Vec<(PathBuf, String)> = Vec::new();
    let mut deadline: Option<tokio::time::Instant> = None;
    // The single retry a refusal earned, with the names it holds.
    let mut retry: Option<(tokio::time::Instant, Vec<(PathBuf, String)>)> = None;
    loop {
        // Copied out so the timer futures borrow nothing the other arms assign to.
        let due = deadline;
        let retry_due = retry.as_ref().map(|(at, _)| *at);
        tokio::select! {
            msg = rx.recv() => match msg {
                None => return,
                Some(ToastMsg::Ready { root, name }) => {
                    match pending.iter_mut().find(|(p, _)| *p == root) {
                        Some(slot) => slot.1 = name,
                        None => pending.push((root, name)),
                    }
                    deadline = Some(tokio::time::Instant::now() + TOAST_DELAY);
                }
                Some(ToastMsg::Drop(root)) => {
                    pending.retain(|(p, _)| *p != root);
                    if pending.is_empty() {
                        deadline = None;
                    }
                    // An ack that lands mid-retry withdraws that root from the retry too.
                    if let Some((_, held)) = &mut retry {
                        held.retain(|(p, _)| *p != root);
                        if held.is_empty() {
                            retry = None;
                        }
                    }
                }
            },
            _ = due_at(due) => {
                deadline = None;
                let names = std::mem::take(&mut pending);
                if names.is_empty() {
                    continue;
                }
                if let Some(held) = show_window(&transport, &names, &updates).await {
                    retry = Some((tokio::time::Instant::now() + TOAST_RETRY, held));
                }
            }
            _ = due_at(retry_due) => {
                let Some((_, names)) = retry.take() else {
                    continue;
                };
                // Ruling 5: this is the second request and there is never a third, so the
                // verdict of the retry is read for its log line and nothing else.
                let _ = show_window(&transport, &names, &updates).await;
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

/// Whether `event` means the *set of repos* may have moved and a discovery rescan is due
/// (deliverable 7). Only the worktree events qualify: herdr's own `worktree.*` are the one
/// notice we get that a checkout appeared or went away. The event is a trigger and nothing
/// more — `roots::discover` decides what is really there, so a `Removed` path still on disk
/// stays a root.
pub fn triggers_rescan(event: &HerdrEvent) -> bool {
    matches!(event, HerdrEvent::WorktreeChanged { .. })
}

/// Cache builders shared by this module's tests and `app.rs`'s.
#[cfg(test)]
pub(crate) mod testfix {
    use super::*;
    use lastcall_engine::herdr::client::PaneRecord;
    use lastcall_engine::herdr::wire::{PaneInfo, WorkspaceInfo};
    use serde_json::json;

    /// One agent-bearing pane. `status` is the *snapshot's* field; [`streaming`] adds the
    /// per-pane stream's newer word, which `derive` must prefer.
    pub fn pane(
        pane_id: &str,
        workspace: &str,
        cwd: &str,
        agent: Option<&str>,
        status: &str,
    ) -> PaneRecord {
        let info: PaneInfo = serde_json::from_value(json!({
            "pane_id": pane_id,
            "terminal_id": "t",
            "workspace_id": workspace,
            "tab_id": "tab",
            "focused": false,
            "agent_status": status,
            "revision": 0,
            "agent": agent,
            "cwd": cwd,
        }))
        .expect("pane fixture parses");
        PaneRecord {
            info,
            provisional: false,
            status: None,
        }
    }

    /// The same pane with the per-pane status stream having said `stream` since.
    pub fn streaming(mut record: PaneRecord, stream: &str) -> PaneRecord {
        record.status = Some(serde_json::from_value(json!(stream)).expect("status parses"));
        record
    }

    /// A pane whose `foreground_cwd` differs from its `cwd` (a `cd` inside the shell).
    pub fn foregrounded(mut record: PaneRecord, foreground_cwd: &str) -> PaneRecord {
        record.info.foreground_cwd = Some(foreground_cwd.to_owned());
        record
    }

    pub fn cache(panes: Vec<PaneRecord>) -> Cache {
        let mut cache = Cache {
            version: "0.8.2".to_owned(),
            protocol: 21,
            ..Cache::default()
        };
        for pane in panes {
            cache.panes.insert(pane.info.pane_id.clone(), pane);
        }
        cache
    }

    /// A workspace, optionally with the worktree provenance §6.6 prefers.
    pub fn workspace(id: &str, label: &str, checkout: Option<&str>) -> WorkspaceInfo {
        serde_json::from_value(json!({
            "workspace_id": id,
            "label": label,
            "worktree": checkout.map(|c| json!({
                "repo_key": "k", "repo_name": "repo", "repo_root": c, "checkout_path": c,
            })),
        }))
        .expect("workspace fixture parses")
    }

    pub fn meta(path: &str, badge: Option<Badge>) -> RootMeta {
        let path = PathBuf::from(path);
        RootMeta {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            parent: path.parent().unwrap_or(Path::new("/")).to_path_buf(),
            path_shown: path.to_string_lossy().into_owned(),
            path,
            kind: lastcall_engine::store::RootKind::Git,
            badge,
            branch: Some("main".to_owned()),
            head: None,
            in_progress: None,
            remote: None,
        }
    }

    pub fn agents(status: Attention, n: u32, pane: &str, agent: &str) -> RootAgents {
        RootAgents {
            status,
            agents: n,
            pane: Some(pane.to_owned()),
            agent: Some(agent.to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testfix::*;
    use super::*;
    use lastcall_testkit::mock_herdr::{InMemoryHerdr, MockControl};
    use serde_json::{Value, json};

    const A: &str = "/W/alpha";
    const B: &str = "/W/beta";
    const NESTED: &str = "/W/alpha/vendor/nested";

    fn roots() -> Vec<RootMeta> {
        vec![meta(A, None), meta(B, None), meta(NESTED, None)]
    }

    /// The rollup priority is `blocked > done > working > idle > unknown` (deliverable 4),
    /// and the derived `Ord` on `Attention` *is* that rule — so this is the test for it.
    #[test]
    fn herdr_attention_order_is_the_rollup_priority() {
        let mut all = vec![
            Attention::Done,
            Attention::Unknown,
            Attention::Blocked,
            Attention::Idle,
            Attention::Working,
        ];
        all.sort();
        assert_eq!(
            all,
            vec![
                Attention::Unknown,
                Attention::Idle,
                Attention::Working,
                Attention::Done,
                Attention::Blocked,
            ]
        );
        assert_eq!(Attention::of(&AgentStatus::Done), Attention::Done);
        assert_eq!(
            Attention::of(&AgentStatus::Unknown("martian".to_owned())),
            Attention::Unknown,
            "a status the server invented is not an alert"
        );
    }

    /// The §6.6 association: agent-bearing panes only, the deepest root wins, a pane under
    /// no root is ignored, and the rollup keeps the pane id of the agent that won it.
    #[test]
    fn herdr_derive_rolls_up_agents_onto_the_deepest_root() {
        let cache = cache(vec![
            pane("p1", "w1", A, Some("claude"), "working"),
            pane("p2", "w1", A, Some("codex"), "done"),
            pane("p3", "w1", A, None, "done"),
            pane("p4", "w1", NESTED, Some("claude"), "blocked"),
            pane("p5", "w1", "/elsewhere", Some("claude"), "done"),
        ]);
        let derived = derive(&cache, &roots());
        assert_eq!(
            derived.keys().collect::<Vec<_>>(),
            vec![&PathBuf::from(A), &PathBuf::from(NESTED)],
            "a pane under no root is ignored, and the nested repo is its own root"
        );
        let alpha = &derived[Path::new(A)];
        assert_eq!(alpha.agents, 2, "the agentless pane does not count");
        assert_eq!(alpha.status, Attention::Done);
        assert_eq!(alpha.pane.as_deref(), Some("p2"), "the winner's pane id");
        assert_eq!(alpha.agent.as_deref(), Some("codex"));
        assert_eq!(derived[Path::new(NESTED)].status, Attention::Blocked);
    }

    /// Two agents at the winning level: the first keeps the jump target, so `g` does not
    /// flap between panes on every re-derivation.
    #[test]
    fn herdr_derive_keeps_the_first_agent_at_the_winning_level() {
        let cache = cache(vec![
            pane("p1", "w1", A, Some("claude"), "done"),
            pane("p2", "w1", A, Some("codex"), "done"),
        ]);
        assert_eq!(
            derive(&cache, &roots())[Path::new(A)].pane.as_deref(),
            Some("p1")
        );
    }

    /// §5.7: the per-pane stream is the live truth; the snapshot's own field is only the
    /// fallback until the first frame lands. And `foreground_cwd` beats `cwd`.
    #[test]
    fn herdr_derive_prefers_the_status_stream_and_the_foreground_cwd() {
        let cache = cache(vec![foregrounded(
            streaming(pane("p1", "w1", A, Some("claude"), "working"), "done"),
            NESTED,
        )]);
        let derived = derive(&cache, &roots());
        assert_eq!(
            derived.keys().collect::<Vec<_>>(),
            vec![&PathBuf::from(NESTED)]
        );
        assert_eq!(derived[Path::new(NESTED)].status, Attention::Done);
    }

    // --- staging (deliverable 6) ---------------------------------------------------------

    /// The exact request. The markers have to be inside `text` — herdr passes the bytes
    /// through and the *application* on the far side is what interprets them (F7) — and the
    /// separator that ends the payload sits inside them too, so it is paste and not the Enter
    /// we are deliberately not pressing.
    #[tokio::test]
    async fn herdr_stage_wraps_the_export_in_bracketed_paste_markers() {
        let mock = InMemoryHerdr::builder()
            .canned(wire::method::PANE_SEND_TEXT, json!({"type": "ok"}))
            .in_memory();
        let control = mock.control();
        stage(&mock, "p1", "echo ONE\necho TWO").await.unwrap();
        let req = control.requests().pop().expect("one request");
        assert_eq!(req.method, "pane.send_text");
        assert_eq!(
            req.params,
            json!({
                "pane_id": "p1",
                "text": "\u{1b}[200~echo ONE\necho TWO\n\n\u{1b}[201~",
            })
        );
        let text = req.params["text"].as_str().unwrap();
        assert!(
            text.ends_with(&format!("{STAGE_TAIL}{BRACKETED_PASTE_END}")),
            "the separator is inside the markers: {text:?}"
        );
        assert!(
            !text.ends_with('\n'),
            "a newline after the markers would submit it"
        );
    }

    /// `pane_not_found` reaches the caller as the reason the status line prints; the flag is
    /// already on disk, so a failed send loses nothing.
    #[tokio::test]
    async fn herdr_stage_reports_a_pane_that_is_gone() {
        let mock = InMemoryHerdr::builder()
            .error(
                wire::method::PANE_SEND_TEXT,
                "pane_not_found",
                "no such pane",
            )
            .in_memory();
        let err = stage(&mock, "gone", "x").await.unwrap_err();
        assert!(err.contains("pane_not_found"), "{err}");
    }

    // --- the picker's candidates (deliverable 6) -----------------------------------------

    /// Every agent-bearing pane under the root, not just the one that wins the rollup, and
    /// across workspaces (F15) — sorted by attention, then label.
    #[test]
    fn herdr_agents_for_lists_every_pane_under_the_root_sorted_by_attention() {
        let mut cache = cache(vec![
            pane("p1", "w1", A, Some("claude"), "idle"),
            pane("p2", "w2", A, Some("zed"), "blocked"),
            pane("p3", "w1", A, Some("codex"), "idle"),
            // Not candidates: another root, a nested root, and a pane with no agent.
            pane("p4", "w1", B, Some("claude"), "blocked"),
            pane("p5", "w1", NESTED, Some("claude"), "blocked"),
            pane("p6", "w1", A, None, "blocked"),
        ]);
        cache
            .workspaces
            .insert("w1".to_owned(), workspace("w1", "alpha", None));
        cache
            .workspaces
            .insert("w2".to_owned(), workspace("w2", "review", None));
        let got = agents_for(&cache, &roots(), Path::new(A), None);
        assert_eq!(
            got.iter()
                .map(|c| (c.pane_id.as_str(), c.label.as_str(), c.status))
                .collect::<Vec<_>>(),
            vec![
                ("p2", "zed", Attention::Blocked),
                ("p1", "claude", Attention::Idle),
                ("p3", "codex", Attention::Idle),
            ],
            "blocked first, then by label"
        );
        assert_eq!(got[0].workspace_label, "review");
        assert_eq!(got[1].workspace_label, "alpha");
        // Scoped to one workspace, the way the nav narrows under `w`.
        let scoped = agents_for(&cache, &roots(), Path::new(A), Some("w1"));
        assert_eq!(
            scoped
                .iter()
                .map(|c| c.pane_id.as_str())
                .collect::<Vec<_>>(),
            vec!["p1", "p3"]
        );
        // The nested root's own pane is its own candidate, never the parent's.
        assert_eq!(
            agents_for(&cache, &roots(), Path::new(NESTED), None)
                .iter()
                .map(|c| c.pane_id.as_str())
                .collect::<Vec<_>>(),
            vec!["p5"]
        );
    }

    /// `foreground_cwd` beats `cwd` here too, and the status stream beats the snapshot —
    /// the walk is `derive`'s, only the keep rule differs.
    #[test]
    fn herdr_agents_for_uses_the_same_walk_as_derive() {
        let cache = cache(vec![foregrounded(
            streaming(pane("p1", "w1", B, Some("claude"), "working"), "blocked"),
            A,
        )]);
        assert!(agents_for(&cache, &roots(), Path::new(B), None).is_empty());
        let got = agents_for(&cache, &roots(), Path::new(A), None);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].status, Attention::Blocked);
    }

    /// Ruling 1: provenance first. The workspace's `checkout_path` anchors the scope and
    /// every root whose badge links it comes along — a pane sitting in an unrelated repo
    /// does not widen it.
    #[test]
    fn herdr_scope_prefers_provenance_over_pane_cwd() {
        let mut cache = cache(vec![pane("p1", "w1", B, Some("claude"), "working")]);
        cache
            .workspaces
            .insert("w1".to_owned(), workspace("w1", "alpha", Some(A)));
        let roots = vec![
            meta(A, None),
            meta(B, None),
            meta(NESTED, Some(Badge::WorktreeOf(PathBuf::from(A)))),
        ];
        let scope = derive_scope(&cache, &roots, "w1").expect("provenance places the workspace");
        assert_eq!(scope.label, "alpha");
        assert_eq!(
            scope.roots,
            [PathBuf::from(A), PathBuf::from(NESTED)]
                .into_iter()
                .collect::<BTreeSet<_>>(),
            "the checkout plus what its badges link, and not beta"
        );
    }

    /// A watched folder inside a scoped repository belongs to that repository's work: the
    /// sponsor's Phase 12 run opened a pane in a repository whose `z_ignore` and
    /// `z_ignore/research` were watched, and the scope hid both. Both arms of the
    /// derivation keep them; a watched folder inside an unscoped repository stays hidden.
    #[test]
    fn herdr_scope_keeps_the_watched_folders_inside_a_scoped_root() {
        let draft = |path: &str| RootMeta {
            kind: lastcall_engine::store::RootKind::Draft,
            ..meta(path, None)
        };
        let roots = vec![
            meta(A, None),
            meta(B, None),
            // Child before parent: the inner folder joins only once the outer one has.
            draft("/W/alpha/z_ignore/research"),
            draft("/W/alpha/z_ignore"),
            draft("/W/beta/z_ignore"),
            draft("/W/alphabet"),
        ];
        let want: BTreeSet<PathBuf> = [A, "/W/alpha/z_ignore", "/W/alpha/z_ignore/research"]
            .into_iter()
            .map(PathBuf::from)
            .collect();

        let mut provenance = cache(vec![]);
        provenance
            .workspaces
            .insert("w1".to_owned(), workspace("w1", "alpha", Some(A)));
        let scope = derive_scope(&provenance, &roots, "w1").expect("provenance");
        assert_eq!(scope.roots, want, "the provenance arm");

        let mut fallback = cache(vec![pane("p1", "w1", A, None, "idle")]);
        fallback
            .workspaces
            .insert("w1".to_owned(), workspace("w1", "alpha", None));
        let scope = derive_scope(&fallback, &roots, "w1").expect("pane cwd");
        assert_eq!(scope.roots, want, "the pane-cwd arm");
    }

    /// The deepest root around a watched folder decides: inside a nested repository it
    /// follows that repository, in with it under provenance and out with it when only a
    /// pane in the outer repository placed the scope.
    #[test]
    fn herdr_scope_watched_folder_follows_the_repository_around_it() {
        let inner = "/W/alpha/vendor/nested/z_ignore";
        let roots = vec![
            meta(A, None),
            meta(NESTED, Some(Badge::NestedIn(PathBuf::from(A)))),
            RootMeta {
                kind: lastcall_engine::store::RootKind::Draft,
                ..meta(inner, None)
            },
        ];
        let mut provenance = cache(vec![]);
        provenance
            .workspaces
            .insert("w1".to_owned(), workspace("w1", "alpha", Some(A)));
        let scope = derive_scope(&provenance, &roots, "w1").expect("provenance");
        assert!(
            scope.roots.contains(Path::new(inner)),
            "in with the nested repository"
        );

        let mut fallback = cache(vec![pane("p1", "w1", A, None, "idle")]);
        fallback
            .workspaces
            .insert("w1".to_owned(), workspace("w1", "alpha", None));
        let scope = derive_scope(&fallback, &roots, "w1").expect("pane cwd");
        assert!(
            !scope.roots.contains(Path::new(inner)),
            "out with the nested repository"
        );
    }

    /// A pane sitting inside a watched folder scopes to that folder and to the watched
    /// folders inside it, and does not reach up to the repository around it.
    #[test]
    fn herdr_scope_from_inside_a_watched_folder_does_not_reach_up() {
        let draft = |path: &str| RootMeta {
            kind: lastcall_engine::store::RootKind::Draft,
            ..meta(path, None)
        };
        let roots = vec![
            meta(A, None),
            draft("/W/alpha/z_ignore"),
            draft("/W/alpha/z_ignore/research"),
        ];
        let mut cache = cache(vec![pane("p1", "w1", "/W/alpha/z_ignore", None, "idle")]);
        cache
            .workspaces
            .insert("w1".to_owned(), workspace("w1", "notes", None));
        let scope = derive_scope(&cache, &roots, "w1").expect("pane cwd");
        assert_eq!(
            scope.roots,
            ["/W/alpha/z_ignore", "/W/alpha/z_ignore/research"]
                .into_iter()
                .map(PathBuf::from)
                .collect::<BTreeSet<_>>()
        );
    }

    /// Deliverable 8 says the `checkout_path` compare is "(canonicalised)". herdr reports
    /// the path the user typed; the engine's roots came through `roots::discover`, which
    /// resolved every symlink — so the raw compare finds nothing and the workspace, whose
    /// whole provenance is right there, silently gets no scope at all.
    #[test]
    fn herdr_scope_places_a_symlinked_checkout_path() {
        let tmp = lastcall_testkit::tmp::TempDir::new("lc-scope-symlink");
        let real = tmp.join("real");
        std::fs::create_dir_all(real.join("alpha")).expect("the real checkout");
        let link = tmp.join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink the parent");
        // What the engine holds: fully resolved, as `roots::discover` leaves it.
        let resolved = std::fs::canonicalize(real.join("alpha")).expect("canonicalize");
        let roots = vec![meta(&resolved.to_string_lossy(), None)];
        // What herdr reports: the path through the symlink, with a trailing separator and
        // a `.` for good measure — all three of which the raw compare failed on.
        let reported = format!("{}/./alpha/", link.display());

        let mut cache = cache(vec![]);
        cache
            .workspaces
            .insert("w1".to_owned(), workspace("w1", "alpha", Some(&reported)));
        let scope = derive_scope(&cache, &roots, "w1").expect("the checkout is a known root");
        assert_eq!(
            scope.roots,
            [resolved.clone()].into_iter().collect::<BTreeSet<_>>()
        );

        // The pure half is unchanged: with an identity resolver the reported path places
        // nothing, and a checkout that is not on disk falls back to its cleaned value.
        assert_eq!(
            derive_scope_with(&cache, &roots, "w1", |raw| PathBuf::from(raw)),
            None,
            "no resolution, no anchor — the defect, pinned"
        );
        assert_eq!(
            canonical_checkout("/gone/./alpha/"),
            PathBuf::from("/gone/alpha")
        );
        assert_eq!(canonical_checkout(&reported), resolved);
    }

    /// No provenance we can place: fall back to where this workspace's panes actually are,
    /// and to no scope at all (no notice, nothing hidden) when even that is empty.
    #[test]
    fn herdr_scope_falls_back_to_pane_cwd_then_to_nothing() {
        let mut cache = cache(vec![
            pane("p1", "w1", B, Some("claude"), "working"),
            pane("p2", "w2", A, Some("claude"), "working"),
        ]);
        cache
            .workspaces
            .insert("w1".to_owned(), workspace("w1", "", Some("/not/a/root")));
        let scope = derive_scope(&cache, &roots(), "w1").expect("containment finds beta");
        assert_eq!(
            scope.label, "w1",
            "an unlabelled workspace is named by its id"
        );
        assert_eq!(
            scope.roots,
            [PathBuf::from(B)].into_iter().collect::<BTreeSet<_>>()
        );

        cache.panes.clear();
        assert_eq!(derive_scope(&cache, &roots(), "w1"), None);
        assert_eq!(
            derive_scope(&cache, &roots(), "nobody"),
            None,
            "a workspace id the snapshot does not know scopes nothing"
        );
    }

    /// The Gate 8 sponsor run's launch flash: lastcall's own pane reported a
    /// `foreground_cwd` that followed the `git` children of the scans, and the containment
    /// fallback narrowed the scope to whichever root was being scanned. Scrubbed, our pane
    /// still counts — by its shell `cwd` — and every other pane keeps its foreground.
    #[test]
    fn herdr_own_pane_foreground_never_steers_the_scope() {
        let mut cache = cache(vec![
            pane("p1", "w2", B, None, "idle"),
            // Our pane: the shell sits above the roots, a scan is running in alpha.
            foregrounded(pane("p2", "w2", "/W", None, "idle"), A),
        ]);
        // A workspace with no worktree provenance: the containment fallback decides.
        cache
            .workspaces
            .insert("w2".to_owned(), workspace("w2", "dev/git parent", None));
        let of = |cache: &Cache| derive_scope(cache, &roots(), "w2").map(|s| s.roots);
        let both: BTreeSet<PathBuf> = [PathBuf::from(A), PathBuf::from(B)].into_iter().collect();
        let beta: BTreeSet<PathBuf> = [PathBuf::from(B)].into_iter().collect();
        assert_eq!(of(&cache), Some(both), "raw, our own scan steers the scope");

        let scrubbed = own_pane_scrubbed(&cache, Some("p2"));
        assert!(matches!(scrubbed, Cow::Owned(_)));
        assert_eq!(
            of(&scrubbed),
            Some(beta),
            "scrubbed, only the other pane places it"
        );
        assert_eq!(
            scrubbed.panes["p2"].info.cwd.as_deref(),
            Some("/W"),
            "the shell cwd is untouched"
        );
        assert_eq!(
            cache.panes["p2"].info.foreground_cwd.as_deref(),
            Some(A),
            "the caller's cache is not mutated"
        );

        // Nothing to scrub borrows: no own pane, an own pane the snapshot does not know,
        // an own pane with no foreground of its own.
        for own in [None, Some("nobody"), Some("p1")] {
            assert!(
                matches!(own_pane_scrubbed(&cache, own), Cow::Borrowed(_)),
                "{own:?}"
            );
        }
    }

    /// Ruling 9 + ruling 10: `done` opens exactly one episode; an ack survives further
    /// derivations that still say `done`; any non-`done` rollup ends it, so the next `done`
    /// alerts again. Only the opened edges are toast-worthy.
    #[test]
    fn herdr_ready_episode_opens_once_and_a_non_done_rollup_ends_it() {
        let mut view = HerdrView::default();
        let done = || BTreeMap::from([(PathBuf::from(A), agents(Attention::Done, 1, "p1", "cl"))]);
        let delta = view.apply_roots(done());
        assert_eq!(delta.opened, vec![PathBuf::from(A)]);
        assert_eq!(
            view.flag(Path::new(A)).unwrap().ready,
            Some(Ready { acked: false })
        );

        // A second `done` derivation is the same episode: no second alert.
        assert_eq!(view.apply_roots(done()), ReadyDelta::default());

        assert!(view.ack(Path::new(A)));
        assert!(!view.ack(Path::new(A)), "acking twice changes nothing");
        assert_eq!(view.apply_roots(done()).opened, Vec::<PathBuf>::new());
        assert_eq!(
            view.flag(Path::new(A)).unwrap().ready,
            Some(Ready { acked: true }),
            "the ack survives a re-derivation that still says done"
        );

        let working =
            BTreeMap::from([(PathBuf::from(A), agents(Attention::Working, 1, "p1", "cl"))]);
        assert_eq!(view.apply_roots(working).closed, vec![PathBuf::from(A)]);
        assert_eq!(view.flag(Path::new(A)).unwrap().ready, None);
        assert_eq!(
            view.apply_roots(done()).opened,
            vec![PathBuf::from(A)],
            "the next done is a new episode and alerts again"
        );

        // A root that loses its agents entirely also ends its episode.
        assert_eq!(
            view.apply_roots(BTreeMap::new()).closed,
            vec![PathBuf::from(A)]
        );
    }

    /// Deliverable 6(b): herdr re-derives on every snapshot, most of which say exactly what
    /// the last one said. `ReadyDelta::changed` is what separates the two, so an unchanged
    /// re-derivation costs no frame; without it the TUI repainted on every poll.
    #[test]
    fn herdr_apply_roots_reports_changed_only_when_the_map_moved() {
        let mut view = HerdrView::default();
        let done = || BTreeMap::from([(PathBuf::from(A), agents(Attention::Done, 1, "p1", "cl"))]);

        assert!(
            !view.apply_roots(BTreeMap::new()).changed,
            "nothing to nothing is not a change"
        );
        assert!(view.apply_roots(done()).changed, "the first root is one");
        assert!(
            !view.apply_roots(done()).changed,
            "the same map again draws nothing"
        );

        // The ack lives in the flag, so the next identical derivation is still no change.
        assert!(view.ack(Path::new(A)));
        assert!(!view.apply_roots(done()).changed);

        // A status change, an agent-count change, and a root arriving or leaving all move it.
        assert!(
            view.apply_roots(BTreeMap::from([(
                PathBuf::from(A),
                agents(Attention::Working, 1, "p1", "cl"),
            )]))
            .changed
        );
        assert!(
            view.apply_roots(BTreeMap::from([(
                PathBuf::from(A),
                agents(Attention::Working, 2, "p1", "cl"),
            )]))
            .changed,
            "a second agent on the same root is a change"
        );
        assert!(
            view.apply_roots(BTreeMap::from([
                (PathBuf::from(A), agents(Attention::Working, 2, "p1", "cl")),
                (PathBuf::from(B), agents(Attention::Idle, 1, "p2", "cl")),
            ]))
            .changed,
            "a root arriving is a change"
        );
        assert!(
            view.apply_roots(BTreeMap::from([(
                PathBuf::from(A),
                agents(Attention::Working, 2, "p1", "cl"),
            )]))
            .changed,
            "a root leaving is a change"
        );
    }

    /// Ruling 4: `done` and `blocked` are worth listing a repo with nothing pending;
    /// `working`, `idle` and `unknown` only annotate one that is listed already.
    #[test]
    fn herdr_only_ready_and_blocked_list_a_repo_on_their_own() {
        for (status, listed) in [
            (Attention::Done, true),
            (Attention::Blocked, true),
            (Attention::Working, false),
            (Attention::Idle, false),
            (Attention::Unknown, false),
        ] {
            let mut view = HerdrView::default();
            view.apply_roots(BTreeMap::from([(
                PathBuf::from(A),
                agents(status, 1, "p1", "cl"),
            )]));
            assert_eq!(
                view.flag(Path::new(A)).unwrap().attention(),
                listed,
                "{status:?}"
            );
        }
    }

    /// §6.6 degradation: a link that is not live has no current data, so every dot goes
    /// neutral rather than lying about a status nobody is refreshing.
    #[test]
    fn herdr_dots_go_neutral_while_the_link_is_not_live() {
        let mut view = HerdrView::default();
        view.apply_roots(BTreeMap::from([
            (PathBuf::from(A), agents(Attention::Done, 1, "p1", "cl")),
            (PathBuf::from(B), agents(Attention::Blocked, 1, "p2", "cl")),
            (
                PathBuf::from(NESTED),
                agents(Attention::Idle, 1, "p3", "cl"),
            ),
        ]));
        view.link = Link::Connected {
            version: "0.8.2".to_owned(),
        };
        assert_eq!(view.dot(Path::new(A)), Some(Dot::Ready { acked: false }));
        view.ack(Path::new(A));
        assert_eq!(view.dot(Path::new(A)), Some(Dot::Ready { acked: true }));
        assert_eq!(view.dot(Path::new(B)), Some(Dot::Blocked));
        assert_eq!(view.dot(Path::new(NESTED)), None, "idle draws nothing");
        for link in [
            Link::Reconnecting,
            Link::Standalone {
                reason: String::new(),
            },
            Link::Off,
        ] {
            view.link = link;
            assert_eq!(view.dot(Path::new(A)), None);
            assert_eq!(view.dot(Path::new(B)), None);
        }
    }

    /// `w` only bites while a scope was derived, and it hides exactly the roots outside it.
    #[test]
    fn herdr_scope_gates_membership_only_while_it_is_on() {
        let mut view = HerdrView::default();
        assert!(view.in_scope(Path::new(B)), "no scope hides nothing");
        view.scoped = true;
        view.scope = Some(Scope {
            label: "alpha".to_owned(),
            roots: [PathBuf::from(A)].into_iter().collect(),
        });
        assert!(view.in_scope(Path::new(A)));
        assert!(!view.in_scope(Path::new(B)));
        view.scoped = false;
        assert!(view.in_scope(Path::new(B)), "w shows all");
        assert_eq!(view.active_scope(), None);
    }

    /// §5.9: a repo name is untrusted text. Control characters and newlines cannot forge a
    /// second line, runs of whitespace fold, and both fields are capped.
    #[test]
    fn herdr_toast_text_sanitises_and_caps() {
        let (title, body) = toast_text(&["al\npha\u{7}  two".to_owned()]);
        assert_eq!(title, "lastcall: al pha two ready for review");
        assert_eq!(body, "al pha two");

        let (title, body) = toast_text(&["alpha".to_owned(), "beta".to_owned()]);
        assert_eq!(title, "lastcall: 2 repos ready for review");
        assert_eq!(body, "alpha, beta");

        let long = "x".repeat(500);
        let (title, body) = toast_text(&[long.clone(), long]);
        assert!(
            title.chars().count() <= TOAST_TITLE_MAX,
            "the count form never grows: {title}"
        );
        assert_eq!(body.chars().count(), TOAST_BODY_MAX);
        assert!(body.ends_with('…'));
    }

    // --- the toast task, on a paused clock (deliverable 6) -------------------------------

    fn shown() -> Value {
        json!({"shown": true, "reason": ""})
    }

    fn refused(reason: &str) -> Value {
        json!({"shown": false, "reason": reason})
    }

    /// The task, with a mock whose `notification.show` answers `results` in order.
    #[allow(clippy::type_complexity)]
    fn toaster(
        results: Vec<Value>,
    ) -> (
        MockControl,
        mpsc::UnboundedSender<ToastMsg>,
        mpsc::UnboundedReceiver<HerdrUpdate>,
        tokio::task::JoinHandle<()>,
    ) {
        let mock = InMemoryHerdr::builder()
            .canned_seq(wire::method::NOTIFICATION_SHOW, results)
            .in_memory();
        let control = mock.control();
        let (tx, rx) = mpsc::unbounded_channel();
        let (utx, urx) = mpsc::unbounded_channel();
        (control, tx, urx, tokio::spawn(toast_loop(mock, rx, utx)))
    }

    /// When each `notification.show` arrived, on the virtual clock.
    fn shows(control: &MockControl) -> Vec<Duration> {
        control
            .requests()
            .into_iter()
            .filter(|r| r.method == wire::method::NOTIFICATION_SHOW)
            .map(|r| r.at)
            .collect()
    }

    /// A `Ready` for a root path, named the way the reducer names it (the basename).
    fn ready_at(path: &str) -> ToastMsg {
        let root = PathBuf::from(path);
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        ToastMsg::Ready { root, name }
    }

    fn drop_at(path: &str) -> ToastMsg {
        ToastMsg::Drop(PathBuf::from(path))
    }

    fn last_params(control: &MockControl) -> Value {
        control
            .requests()
            .into_iter()
            .rfind(|r| r.method == wire::method::NOTIFICATION_SHOW)
            .expect("a notification.show")
            .params
    }

    /// Ruling 5: everything that goes ready inside the 7 s window is one notification, and
    /// the window is trailing — a second name inside it moves the deadline out.
    #[tokio::test(start_paused = true)]
    async fn herdr_toast_coalesces_a_window_into_one_notification() {
        let (control, tx, mut updates, task) = toaster(vec![shown()]);
        tx.send(ready_at("/W/alpha")).unwrap();
        tokio::time::sleep(TOAST_DELAY / 2).await;
        tx.send(ready_at("/W/beta")).unwrap();
        tx.send(ready_at("/W/beta")).unwrap();
        tokio::time::sleep(TOAST_DELAY * 2).await;

        assert_eq!(shows(&control).len(), 1, "one window, one request");
        let params = last_params(&control);
        assert_eq!(params["title"], "lastcall: 2 repos ready for review");
        assert_eq!(params["body"], "alpha, beta", "each name once, in order");
        assert_eq!(params["sound"], "done");
        assert_eq!(
            updates.recv().await,
            Some(HerdrUpdate::Toast(Ok(ToastShown {
                shown: true,
                reason: String::new()
            })))
        );
        drop(tx);
        task.await.unwrap();
    }

    /// An ack inside the window withdraws that name; when it was the only one, no request
    /// is sent at all — the user has already seen it.
    #[tokio::test(start_paused = true)]
    async fn herdr_toast_drops_an_acked_name_and_sends_nothing_when_empty() {
        let (control, tx, _updates, task) = toaster(vec![shown()]);
        tx.send(ready_at("/W/alpha")).unwrap();
        tx.send(ready_at("/W/beta")).unwrap();
        tokio::time::sleep(TOAST_DELAY / 2).await;
        tx.send(drop_at("/W/alpha")).unwrap();
        tokio::time::sleep(TOAST_DELAY * 2).await;
        assert_eq!(shows(&control).len(), 1);
        assert_eq!(
            last_params(&control)["title"],
            "lastcall: beta ready for review"
        );

        control.clear_requests();
        tx.send(ready_at("/W/gamma")).unwrap();
        tokio::time::sleep(TOAST_DELAY / 2).await;
        tx.send(drop_at("/W/gamma")).unwrap();
        tokio::time::sleep(TOAST_DELAY * 3).await;
        assert!(shows(&control).is_empty(), "an emptied window is not sent");
        drop(tx);
        task.await.unwrap();
    }

    /// Ruling 5: `busy` and `rate_limited` earn exactly one retry, 5 s later — and never a
    /// third request, however many times herdr says busy.
    #[tokio::test(start_paused = true)]
    async fn herdr_toast_retries_once_on_busy_and_never_a_third_time() {
        let (control, tx, mut updates, task) = toaster(vec![refused("busy"), refused("busy")]);
        tx.send(ready_at("/W/alpha")).unwrap();
        tokio::time::sleep(TOAST_DELAY + TOAST_RETRY * 4).await;
        let at = shows(&control);
        assert_eq!(at.len(), 2, "one retry, never a third: {at:?}");
        assert_eq!(at[1] - at[0], TOAST_RETRY, "the retry waits exactly 5 s");
        assert!(
            updates.try_recv().is_err(),
            "a refused toast is not a banner"
        );
        drop(tx);
        task.await.unwrap();
    }

    /// Any other verdict is herdr's decision, not a transient: dropped without a retry.
    #[tokio::test(start_paused = true)]
    async fn herdr_toast_drops_a_non_retryable_refusal() {
        let (control, tx, mut updates, task) = toaster(vec![refused("do_not_disturb"), shown()]);
        tx.send(ready_at("/W/alpha")).unwrap();
        tokio::time::sleep(TOAST_DELAY + TOAST_RETRY * 4).await;
        assert_eq!(shows(&control).len(), 1, "no retry");
        assert!(updates.try_recv().is_err());
        drop(tx);
        task.await.unwrap();
    }

    /// Review (b) F5: the window is keyed by path, not by the display name, so two
    /// checkouts that share a basename are two entries — and an ack of one leaves the
    /// other's toast standing.
    #[tokio::test(start_paused = true)]
    async fn herdr_toast_keys_the_window_by_path_not_by_display_name() {
        let (control, tx, _updates, task) = toaster(vec![shown(), shown()]);
        tx.send(ready_at("/A/proj")).unwrap();
        tx.send(ready_at("/B/proj")).unwrap();
        tokio::time::sleep(TOAST_DELAY * 2).await;
        assert_eq!(shows(&control).len(), 1, "one window");
        let params = last_params(&control);
        assert_eq!(params["title"], "lastcall: 2 repos ready for review");
        assert_eq!(
            params["body"], "A/proj, B/proj",
            "two entries, each qualified by its parent"
        );

        control.clear_requests();
        tx.send(ready_at("/A/proj")).unwrap();
        tx.send(ready_at("/B/proj")).unwrap();
        tokio::time::sleep(TOAST_DELAY / 2).await;
        tx.send(drop_at("/A/proj")).unwrap();
        tokio::time::sleep(TOAST_DELAY * 2).await;
        assert_eq!(shows(&control).len(), 1);
        assert_eq!(
            last_params(&control)["title"],
            "lastcall: proj ready for review",
            "acking /A/proj withdraws only /A/proj"
        );
        drop(tx);
        task.await.unwrap();
    }

    /// Review (b) F6 (a): the retry is timer state, so an ack that lands during the 5 s
    /// wait withdraws its root and the retry never goes out.
    #[tokio::test(start_paused = true)]
    async fn herdr_toast_an_ack_during_the_retry_wait_cancels_it() {
        let (control, tx, _updates, task) = toaster(vec![refused("busy"), shown()]);
        tx.send(ready_at("/W/alpha")).unwrap();
        tokio::time::sleep(TOAST_DELAY + Duration::from_secs(1)).await;
        assert_eq!(shows(&control).len(), 1, "the window fired at 7 s");
        tx.send(drop_at("/W/alpha")).unwrap();
        tokio::time::sleep(TOAST_RETRY * 4).await;
        assert_eq!(shows(&control).len(), 1, "an acked root is not retried");
        drop(tx);
        task.await.unwrap();
    }

    /// Review (b) F6 (b): a root that goes ready at 9 s, while the retry for another root
    /// is still waiting, gets its own window at 16 s — not at 19 s, behind the wait.
    #[tokio::test(start_paused = true)]
    async fn herdr_toast_a_ready_during_the_retry_wait_starts_its_window_on_time() {
        let (control, tx, _updates, task) =
            toaster(vec![refused("busy"), refused("busy"), shown()]);
        tx.send(ready_at("/W/alpha")).unwrap();
        tokio::time::sleep(Duration::from_secs(9)).await;
        tx.send(ready_at("/W/beta")).unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;

        let at = shows(&control);
        assert_eq!(at.len(), 3, "alpha, alpha's retry, beta: {at:?}");
        assert_eq!(at[1] - at[0], TOAST_RETRY, "alpha's retry, 5 s after 7 s");
        assert_eq!(
            at[2] - at[0],
            Duration::from_secs(9),
            "beta arrived at 9 s and fires at 16 s: {at:?}"
        );
        assert_eq!(
            last_params(&control)["title"],
            "lastcall: beta ready for review"
        );
        drop(tx);
        task.await.unwrap();
    }

    /// Deliverable 4's trigger set, and the events that only change the badge.
    #[test]
    fn herdr_event_mapping_splits_badge_news_from_rederivation() {
        let connected = HerdrEvent::Connected {
            version: "0.8.2".to_owned(),
            protocol: 21,
        };
        assert_eq!(
            update_of(&connected),
            Some(HerdrUpdate::Connected {
                version: "0.8.2".to_owned(),
                protocol: 21
            })
        );
        assert!(rederives(&connected), "connecting re-derives everything");
        assert_eq!(
            update_of(&HerdrEvent::Disconnected {
                reason: "eof".to_owned()
            }),
            Some(HerdrUpdate::Reconnecting)
        );
        assert_eq!(
            update_of(&HerdrEvent::Standalone {
                notice: "protocol 99".to_owned()
            }),
            Some(HerdrUpdate::Standalone {
                reason: "protocol 99".to_owned()
            })
        );
    }

    /// Deliverable 7: only a worktree event asks the engine to look for new repos. A status
    /// flip moves a dot, and rescanning every parent dir for one would be absurd.
    #[test]
    fn herdr_only_a_worktree_event_asks_for_a_discovery_rescan() {
        use lastcall_engine::herdr::client::{ResyncTarget, WorktreeChange};

        let worktree = |change| HerdrEvent::WorktreeChanged {
            change,
            workspace_id: "w1".to_owned(),
            path: "/tmp/w/alpha-wt".to_owned(),
            branch: Some("wt".to_owned()),
        };
        for change in [
            WorktreeChange::Created,
            WorktreeChange::Opened,
            WorktreeChange::Removed,
        ] {
            let event = worktree(change);
            assert!(triggers_rescan(&event), "{event:?}");
            assert!(!rederives(&event), "the roots moved, not the association");
        }
        for quiet in [
            HerdrEvent::Resync(ResyncTarget::Snapshot),
            HerdrEvent::AgentStatusChanged {
                pane_id: "w1:p1".to_owned(),
                workspace_id: "w1".to_owned(),
                from: Some(AgentStatus::Working),
                to: AgentStatus::Done,
                agent: None,
            },
            HerdrEvent::Disconnected {
                reason: "eof".to_owned(),
            },
        ] {
            assert!(!triggers_rescan(&quiet), "{quiet:?}");
        }
    }
}
