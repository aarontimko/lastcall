//! herdr wire types (docs/spec/00-spec.md §5.1, §5.5, §5.8, §5.9), hand-checked against
//! `docs/next/api/herdr-api.schema.json` in herdr v0.8.2 (commit `5158ada`).
//!
//! Rules that apply to every type in this file (§5.2):
//! - **Never deny unknown fields, anywhere.** herdr adds fields between releases and the server
//!   never rejects a mismatched client; we must ignore what we do not know. (The opposite rule
//!   holds for our own config types, `crate::config`, where the serde deny-unknown-fields
//!   attribute is required — the two are opposite on purpose.)
//! - Every optional field is an `Option` (or a map that defaults to empty).
//! - Unknown `event` strings and unknown `AgentStatus` values deserialize to an
//!   `Unknown(String)` variant; they never fail the parse.
//! - Error responses are detected by the presence of `error` vs `result`; codes are plain
//!   strings matched by name. `not_found` is never matched as a code (§5.1).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Error codes we handle by name (§5.1). Any other code is a generic failure.
pub mod error_code {
    pub const PANE_NOT_FOUND: &str = "pane_not_found";
    pub const WORKSPACE_NOT_FOUND: &str = "workspace_not_found";
    pub const WORKTREE_LIST_FAILED: &str = "worktree_list_failed";
    pub const INVALID_PARAMS: &str = "invalid_params";
    pub const TIMEOUT: &str = "timeout";
    pub const AGENT_BLOCKED: &str = "agent_blocked";
    pub const AGENT_NOT_FOUND: &str = "agent_not_found";
    pub const SERVER_UNAVAILABLE: &str = "server_unavailable";
}

/// Method names we call (§5.9).
pub mod method {
    pub const PING: &str = "ping";
    pub const SESSION_SNAPSHOT: &str = "session.snapshot";
    pub const PANE_GET: &str = "pane.get";
    pub const PANE_LIST: &str = "pane.list";
    pub const WORKTREE_LIST: &str = "worktree.list";
    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";
    /// Phase 5.
    pub const NOTIFICATION_SHOW: &str = "notification.show";
}

/// Errors from parsing wire lines.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("invalid JSON on the wire: {0}")]
    Json(#[from] serde_json::Error),
    #[error("response line has neither `result` nor `error`")]
    NeitherResultNorError,
    #[error("event `{event}` has malformed data: {source}")]
    EventData {
        event: String,
        #[source]
        source: serde_json::Error,
    },
}

// ---------------------------------------------------------------------------------------
// Request / response envelopes
// ---------------------------------------------------------------------------------------

/// `{"id": "<string>", "method": "<name>", "params": {...}}`. `params` is required on every
/// method, including `ping` (`{}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub method: String,
    #[serde(default = "empty_object")]
    pub params: Value,
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}

impl Request {
    pub fn new(id: impl Into<String>, method: impl Into<String>, params: Value) -> Self {
        let params = if params.is_null() {
            empty_object()
        } else {
            params
        };
        Self {
            id: id.into(),
            method: method.into(),
            params,
        }
    }

    /// One JSON line including the trailing newline.
    pub fn to_line(&self) -> Result<String, serde_json::Error> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        Ok(line)
    }
}

/// The error body of an error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    #[serde(default)]
    pub message: String,
}

impl fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// Raw response envelope; exactly one of `result` / `error` is present on a valid line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseLine {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

/// A parsed response: success carries the `result` object, failure the error body.
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    Result(Value),
    Error(ErrorBody),
}

/// Parse one response line. Distinguishes by presence of `result` vs `error` (§5.1).
pub fn parse_response(line: &str) -> Result<(Option<String>, Response), WireError> {
    let raw: ResponseLine = serde_json::from_str(line)?;
    match (raw.result, raw.error) {
        (_, Some(error)) => Ok((raw.id, Response::Error(error))),
        (Some(result), None) => Ok((raw.id, Response::Result(result))),
        (None, None) => Err(WireError::NeitherResultNorError),
    }
}

/// The `type` discriminant of a success result, when present.
pub fn result_type(result: &Value) -> Option<&str> {
    result.get("type").and_then(Value::as_str)
}

// ---------------------------------------------------------------------------------------
// Agent status
// ---------------------------------------------------------------------------------------

/// `AgentStatus` on the wire: `idle | working | blocked | done | unknown` (§5.7). Anything
/// else — including the literal `unknown` — lands in `Unknown(raw)` so a newer herdr never
/// breaks the parse.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown(String),
}

impl AgentStatus {
    pub fn as_str(&self) -> &str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Working => "working",
            AgentStatus::Blocked => "blocked",
            AgentStatus::Done => "done",
            AgentStatus::Unknown(raw) => raw,
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw {
            "idle" => AgentStatus::Idle,
            "working" => AgentStatus::Working,
            "blocked" => AgentStatus::Blocked,
            "done" => AgentStatus::Done,
            other => AgentStatus::Unknown(other.to_string()),
        }
    }
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for AgentStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AgentStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(AgentStatus::parse(&raw))
    }
}

// ---------------------------------------------------------------------------------------
// Payload shapes (§5.8)
// ---------------------------------------------------------------------------------------

/// `PaneInfo` (schema `#/schemas/event/$defs/PaneInfo`, line 760). Required on the wire:
/// `pane_id`, `terminal_id`, `workspace_id`, `tab_id`, `focused`, `agent_status`, `revision`.
/// We consume `agent_status`, `agent`/`display_agent`, `workspace_id`, `cwd`/`foreground_cwd`.
/// `revision` is a presentation-token revision only — never a change detector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    #[serde(default)]
    pub terminal_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub tab_id: String,
    #[serde(default)]
    pub focused: bool,
    #[serde(default = "unknown_status")]
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_title_stripped: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll: Option<Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub state_labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tokens: BTreeMap<String, String>,
}

fn unknown_status() -> AgentStatus {
    AgentStatus::Unknown("unknown".to_string())
}

impl PaneInfo {
    /// The label we show for the agent, if any.
    pub fn agent_label(&self) -> Option<&str> {
        self.display_agent.as_deref().or(self.agent.as_deref())
    }

    /// A pane worth a per-pane status subscription: one that has an agent (§5.4).
    pub fn is_agent_bearing(&self) -> bool {
        self.agent.is_some()
    }
}

/// `WorkspaceInfo` (schema line 1071). `number` is a positional index that renumbers on
/// reorder — key everything on `workspace_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    #[serde(default)]
    pub number: u64,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub pane_count: u64,
    #[serde(default)]
    pub tab_count: u64,
    #[serde(default)]
    pub active_tab_id: String,
    #[serde(default = "unknown_status")]
    pub agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tokens: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorkspaceWorktreeInfo>,
}

/// `WorkspaceWorktreeInfo` (schema line 1136): provenance. There is no `branch` here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceWorktreeInfo {
    pub repo_key: String,
    pub repo_name: String,
    pub repo_root: String,
    pub checkout_path: String,
    #[serde(default)]
    pub is_linked_worktree: bool,
}

/// `TabInfo` (schema line 1032).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabInfo {
    pub tab_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub number: u64,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub pane_count: u64,
    #[serde(default = "unknown_status")]
    pub agent_status: AgentStatus,
}

/// `WorktreeInfo` (schema line 1163). `label` is the repo name; `is_bare`/`is_prunable` are
/// reliable only from `worktree.list` (§5.8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    #[serde(default)]
    pub is_bare: bool,
    #[serde(default)]
    pub is_detached: bool,
    #[serde(default)]
    pub is_prunable: bool,
    #[serde(default)]
    pub is_linked_worktree: bool,
    #[serde(default)]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_workspace_id: Option<String>,
}

/// `AgentInfo` (schema line 6139), the `agents[]` entries of a snapshot. Only the fields we
/// consume; the rest are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default = "unknown_status")]
    pub agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_cwd: Option<String>,
    #[serde(default)]
    pub focused: bool,
}

/// `SessionSnapshot` (schema line 9840). There is no top-level worktrees array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub protocol: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_pane_id: Option<String>,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceInfo>,
    #[serde(default)]
    pub tabs: Vec<TabInfo>,
    #[serde(default)]
    pub panes: Vec<PaneInfo>,
    #[serde(default)]
    pub layouts: Vec<Value>,
    #[serde(default)]
    pub agents: Vec<AgentInfo>,
}

// ---------------------------------------------------------------------------------------
// Results (§5.9)
// ---------------------------------------------------------------------------------------

/// `ping` → `{"type":"pong","version":"0.8.2","protocol":21,"capabilities":{...}|null}`
/// (schema line 8654).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pong {
    pub version: String,
    pub protocol: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Value>,
}

/// `session.snapshot` → `{"type":"session_snapshot","snapshot":{...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotResult {
    pub snapshot: SessionSnapshot,
}

/// `pane.get` → `{"type":"pane","pane":{...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneResult {
    pub pane: PaneInfo,
}

/// `pane.list` → `{"type":"pane_list","panes":[...]}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneListResult {
    #[serde(default)]
    pub panes: Vec<PaneInfo>,
}

/// `worktree.list` → `{"type":"worktree_list","source":{...},"worktrees":[...]}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeListResult {
    #[serde(default)]
    pub worktrees: Vec<WorktreeInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Value>,
}

/// `notification.show` → `{shown, reason}` (Phase 5; defined, unused).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationShowResult {
    #[serde(default)]
    pub shown: bool,
    #[serde(default)]
    pub reason: String,
}

// ---------------------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------------------

/// `pane.get` / `pane.focus` params (schema `PaneTarget`, line 3150).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneTarget {
    pub pane_id: String,
}

/// `worktree.list` params (schema line 4373).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorktreeListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// `notification.show` params (schema line 2298; Phase 5, defined, unused).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationShowParams {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sound: Option<String>,
}

/// One entry of `events.subscribe.params.subscriptions` (schema `Subscription`, line 3682).
/// Subscription *requests* use dotted names; the events they produce arrive snake_case (§5.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
}

impl Subscription {
    pub fn global(kind: &str) -> Self {
        Self {
            kind: kind.to_string(),
            pane_id: None,
        }
    }

    /// The per-pane status subscription. `pane_id` is required — there is no global agent
    /// status stream (§5.4). Subscribed unfiltered on purpose (§5.5).
    pub fn pane_agent_status_changed(pane_id: &str) -> Self {
        Self {
            kind: PANE_AGENT_STATUS_CHANGED.to_string(),
            pane_id: Some(pane_id.to_string()),
        }
    }
}

/// `events.subscribe` params (schema line 2062).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsSubscribeParams {
    pub subscriptions: Vec<Subscription>,
}

/// The dotted per-pane subscription/event name.
pub const PANE_AGENT_STATUS_CHANGED: &str = "pane.agent_status_changed";

/// The lifecycle subscription set, verbatim from §5.4.
pub const LIFECYCLE_SUBSCRIPTIONS: [&str; 15] = [
    "workspace.created",
    "workspace.updated",
    "workspace.closed",
    "workspace.focused",
    "worktree.created",
    "worktree.opened",
    "worktree.removed",
    "pane.created",
    "pane.updated",
    "pane.closed",
    "pane.moved",
    "pane.exited",
    "pane.focused",
    "tab.focused",
    "pane.agent_detected",
];

/// The §5.4 lifecycle set as subscription entries.
pub fn lifecycle_subscriptions() -> Vec<Subscription> {
    LIFECYCLE_SUBSCRIPTIONS
        .iter()
        .map(|kind| Subscription::global(kind))
        .collect()
}

/// The ack line's result type for `events.subscribe`.
pub const SUBSCRIPTION_STARTED: &str = "subscription_started";

// ---------------------------------------------------------------------------------------
// Events (§5.5): two envelope shapes on one stream
// ---------------------------------------------------------------------------------------

/// The raw pushed line: `{"event": "<name>", "data": {...}}`. No `id` — never correlate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventLine {
    pub event: String,
    #[serde(default)]
    pub data: Value,
}

/// The dotted, untagged per-pane status event (schema `PaneAgentStatusChangedEvent`, line
/// 5936).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneAgentStatusChangedEvent {
    pub pane_id: String,
    #[serde(default)]
    pub workspace_id: String,
    pub agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_agent: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub state_labels: BTreeMap<String, String>,
}

/// Every event we consume, parsed from either envelope shape. `event` is parsed as an opaque
/// string and branched on; there is no `#[serde(tag)]` across the union.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    PaneCreated {
        pane: PaneInfo,
    },
    PaneUpdated {
        pane: PaneInfo,
    },
    PaneMoved {
        previous_pane_id: String,
        previous_workspace_id: String,
        previous_tab_id: String,
        pane: PaneInfo,
    },
    PaneClosed {
        pane_id: String,
        workspace_id: String,
    },
    PaneExited {
        pane_id: String,
        workspace_id: String,
    },
    PaneFocused {
        pane_id: String,
        workspace_id: String,
    },
    PaneAgentDetected {
        pane_id: String,
        workspace_id: String,
        agent: Option<String>,
        final_status: Option<AgentStatus>,
        released: bool,
    },
    WorkspaceCreated {
        workspace: WorkspaceInfo,
    },
    WorkspaceUpdated {
        workspace: WorkspaceInfo,
    },
    WorkspaceClosed {
        workspace_id: String,
        workspace: Option<WorkspaceInfo>,
    },
    WorkspaceFocused {
        workspace_id: String,
    },
    TabFocused {
        tab_id: String,
        workspace_id: String,
    },
    WorktreeCreated {
        workspace: WorkspaceInfo,
        worktree: WorktreeInfo,
    },
    WorktreeOpened {
        workspace: WorkspaceInfo,
        worktree: WorktreeInfo,
        already_open: bool,
    },
    WorktreeRemoved {
        workspace_id: String,
        worktree: WorktreeInfo,
        forced: bool,
        workspace: Option<WorkspaceInfo>,
    },
    /// The dotted per-pane status event.
    PaneAgentStatusChanged(PaneAgentStatusChangedEvent),
    /// Anything we do not recognise. The client treats it as "schedule a resync".
    Unknown {
        event: String,
        data: Value,
    },
}

// Private helpers: one per data shape, unknown fields tolerated (the `type` tag is ignored).
#[derive(Deserialize)]
struct PaneData {
    pane: PaneInfo,
}
#[derive(Deserialize)]
struct PaneMovedData {
    previous_pane_id: String,
    #[serde(default)]
    previous_workspace_id: String,
    #[serde(default)]
    previous_tab_id: String,
    pane: PaneInfo,
}
#[derive(Deserialize)]
struct PaneRefData {
    pane_id: String,
    #[serde(default)]
    workspace_id: String,
}
#[derive(Deserialize)]
struct PaneAgentDetectedData {
    pane_id: String,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    final_status: Option<AgentStatus>,
    #[serde(default)]
    released: bool,
}
#[derive(Deserialize)]
struct WorkspaceData {
    workspace: WorkspaceInfo,
}
#[derive(Deserialize)]
struct WorkspaceClosedData {
    workspace_id: String,
    #[serde(default)]
    workspace: Option<WorkspaceInfo>,
}
#[derive(Deserialize)]
struct WorkspaceRefData {
    workspace_id: String,
}
#[derive(Deserialize)]
struct TabFocusedData {
    tab_id: String,
    #[serde(default)]
    workspace_id: String,
}
#[derive(Deserialize)]
struct WorktreeData {
    workspace: WorkspaceInfo,
    worktree: WorktreeInfo,
    #[serde(default)]
    already_open: bool,
}
#[derive(Deserialize)]
struct WorktreeRemovedData {
    workspace_id: String,
    worktree: WorktreeInfo,
    #[serde(default)]
    forced: bool,
    #[serde(default)]
    workspace: Option<WorkspaceInfo>,
}

impl Event {
    /// Parse one pushed line in either envelope shape.
    pub fn parse_line(line: &str) -> Result<Event, WireError> {
        let raw: EventLine = serde_json::from_str(line)?;
        Event::from_event_line(raw)
    }

    /// Branch on the opaque `event` string.
    pub fn from_event_line(raw: EventLine) -> Result<Event, WireError> {
        let EventLine { event, data } = raw;
        fn de<T: serde::de::DeserializeOwned>(event: &str, data: &Value) -> Result<T, WireError> {
            serde_json::from_value(data.clone()).map_err(|source| WireError::EventData {
                event: event.to_string(),
                source,
            })
        }
        let parsed = match event.as_str() {
            "pane_created" => {
                let d: PaneData = de(&event, &data)?;
                Event::PaneCreated { pane: d.pane }
            }
            "pane_updated" => {
                let d: PaneData = de(&event, &data)?;
                Event::PaneUpdated { pane: d.pane }
            }
            "pane_moved" => {
                let d: PaneMovedData = de(&event, &data)?;
                Event::PaneMoved {
                    previous_pane_id: d.previous_pane_id,
                    previous_workspace_id: d.previous_workspace_id,
                    previous_tab_id: d.previous_tab_id,
                    pane: d.pane,
                }
            }
            "pane_closed" => {
                let d: PaneRefData = de(&event, &data)?;
                Event::PaneClosed {
                    pane_id: d.pane_id,
                    workspace_id: d.workspace_id,
                }
            }
            "pane_exited" => {
                let d: PaneRefData = de(&event, &data)?;
                Event::PaneExited {
                    pane_id: d.pane_id,
                    workspace_id: d.workspace_id,
                }
            }
            "pane_focused" => {
                let d: PaneRefData = de(&event, &data)?;
                Event::PaneFocused {
                    pane_id: d.pane_id,
                    workspace_id: d.workspace_id,
                }
            }
            "pane_agent_detected" => {
                let d: PaneAgentDetectedData = de(&event, &data)?;
                Event::PaneAgentDetected {
                    pane_id: d.pane_id,
                    workspace_id: d.workspace_id,
                    agent: d.agent,
                    final_status: d.final_status,
                    released: d.released,
                }
            }
            "workspace_created" => {
                let d: WorkspaceData = de(&event, &data)?;
                Event::WorkspaceCreated {
                    workspace: d.workspace,
                }
            }
            "workspace_updated" => {
                let d: WorkspaceData = de(&event, &data)?;
                Event::WorkspaceUpdated {
                    workspace: d.workspace,
                }
            }
            "workspace_closed" => {
                let d: WorkspaceClosedData = de(&event, &data)?;
                Event::WorkspaceClosed {
                    workspace_id: d.workspace_id,
                    workspace: d.workspace,
                }
            }
            "workspace_focused" => {
                let d: WorkspaceRefData = de(&event, &data)?;
                Event::WorkspaceFocused {
                    workspace_id: d.workspace_id,
                }
            }
            "tab_focused" => {
                let d: TabFocusedData = de(&event, &data)?;
                Event::TabFocused {
                    tab_id: d.tab_id,
                    workspace_id: d.workspace_id,
                }
            }
            "worktree_created" => {
                let d: WorktreeData = de(&event, &data)?;
                Event::WorktreeCreated {
                    workspace: d.workspace,
                    worktree: d.worktree,
                }
            }
            "worktree_opened" => {
                let d: WorktreeData = de(&event, &data)?;
                Event::WorktreeOpened {
                    workspace: d.workspace,
                    worktree: d.worktree,
                    already_open: d.already_open,
                }
            }
            "worktree_removed" => {
                let d: WorktreeRemovedData = de(&event, &data)?;
                Event::WorktreeRemoved {
                    workspace_id: d.workspace_id,
                    worktree: d.worktree,
                    forced: d.forced,
                    workspace: d.workspace,
                }
            }
            // The per-pane subscription emits the dotted, untagged shape. The snake_case
            // spelling is in the schema's EventKind enum too; accept both, same data shape.
            PANE_AGENT_STATUS_CHANGED | "pane_agent_status_changed" => {
                let d: PaneAgentStatusChangedEvent = de(&event, &data)?;
                Event::PaneAgentStatusChanged(d)
            }
            _ => Event::Unknown { event, data },
        };
        Ok(parsed)
    }

    /// The wire event name.
    pub fn name(&self) -> &str {
        match self {
            Event::PaneCreated { .. } => "pane_created",
            Event::PaneUpdated { .. } => "pane_updated",
            Event::PaneMoved { .. } => "pane_moved",
            Event::PaneClosed { .. } => "pane_closed",
            Event::PaneExited { .. } => "pane_exited",
            Event::PaneFocused { .. } => "pane_focused",
            Event::PaneAgentDetected { .. } => "pane_agent_detected",
            Event::WorkspaceCreated { .. } => "workspace_created",
            Event::WorkspaceUpdated { .. } => "workspace_updated",
            Event::WorkspaceClosed { .. } => "workspace_closed",
            Event::WorkspaceFocused { .. } => "workspace_focused",
            Event::TabFocused { .. } => "tab_focused",
            Event::WorktreeCreated { .. } => "worktree_created",
            Event::WorktreeOpened { .. } => "worktree_opened",
            Event::WorktreeRemoved { .. } => "worktree_removed",
            Event::PaneAgentStatusChanged(_) => PANE_AGENT_STATUS_CHANGED,
            Event::Unknown { event, .. } => event,
        }
    }

    /// The pane this event is about, if any.
    pub fn pane_id(&self) -> Option<&str> {
        match self {
            Event::PaneCreated { pane }
            | Event::PaneUpdated { pane }
            | Event::PaneMoved { pane, .. } => Some(&pane.pane_id),
            Event::PaneClosed { pane_id, .. }
            | Event::PaneExited { pane_id, .. }
            | Event::PaneFocused { pane_id, .. }
            | Event::PaneAgentDetected { pane_id, .. } => Some(pane_id),
            Event::PaneAgentStatusChanged(e) => Some(&e.pane_id),
            _ => None,
        }
    }

    /// The workspace this event is about, if any.
    pub fn workspace_id(&self) -> Option<&str> {
        match self {
            Event::PaneCreated { pane }
            | Event::PaneUpdated { pane }
            | Event::PaneMoved { pane, .. } => Some(&pane.workspace_id),
            Event::PaneClosed { workspace_id, .. }
            | Event::PaneExited { workspace_id, .. }
            | Event::PaneFocused { workspace_id, .. }
            | Event::PaneAgentDetected { workspace_id, .. }
            | Event::WorkspaceClosed { workspace_id, .. }
            | Event::WorkspaceFocused { workspace_id }
            | Event::TabFocused { workspace_id, .. }
            | Event::WorktreeRemoved { workspace_id, .. } => Some(workspace_id),
            Event::WorkspaceCreated { workspace }
            | Event::WorkspaceUpdated { workspace }
            | Event::WorktreeCreated { workspace, .. }
            | Event::WorktreeOpened { workspace, .. } => Some(&workspace.workspace_id),
            Event::PaneAgentStatusChanged(e) => Some(&e.workspace_id),
            Event::Unknown { .. } => None,
        }
    }

    /// One-line summary: `event=<name> [ws=<id>] [pane=<id>] [tab=<id>] [...]`.
    pub fn summary(&self) -> String {
        let mut s = format!("event={}", self.name());
        if let Some(ws) = self.workspace_id() {
            s.push_str(&format!(" ws={ws}"));
        }
        if let Some(pane) = self.pane_id() {
            s.push_str(&format!(" pane={pane}"));
        }
        match self {
            Event::TabFocused { tab_id, .. } => s.push_str(&format!(" tab={tab_id}")),
            Event::PaneMoved {
                previous_pane_id, ..
            } => s.push_str(&format!(" from={previous_pane_id}")),
            Event::PaneAgentDetected {
                agent, released, ..
            } => s.push_str(&format!(
                " agent={} released={released}",
                agent.as_deref().unwrap_or("-")
            )),
            Event::WorktreeCreated { worktree, .. }
            | Event::WorktreeOpened { worktree, .. }
            | Event::WorktreeRemoved { worktree, .. } => {
                s.push_str(&format!(" worktree={}", worktree.path));
            }
            Event::PaneAgentStatusChanged(e) => s.push_str(&format!(
                " agent={} status={}",
                e.agent.as_deref().unwrap_or("-"),
                e.agent_status
            )),
            _ => {}
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../lastcall-testkit/fixtures/herdr"
    );

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{FIXTURES}/{name}"))
            .unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    #[test]
    fn wire_request_line_always_carries_params() {
        let r = Request::new("lc-1", "ping", Value::Null);
        assert_eq!(r.params, serde_json::json!({}));
        let line = r.to_line().unwrap();
        assert!(line.ends_with('\n'));
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], "lc-1");
        assert_eq!(v["method"], "ping");
        assert_eq!(v["params"], serde_json::json!({}));
    }

    #[test]
    fn wire_response_detects_result_vs_error() {
        let (id, r) = parse_response(r#"{"id":"a","result":{"type":"ok"}}"#).unwrap();
        assert_eq!(id.as_deref(), Some("a"));
        assert!(matches!(r, Response::Result(v) if result_type(&v) == Some("ok")));

        let (_, r) =
            parse_response(r#"{"id":"b","error":{"code":"pane_not_found","message":"no"}}"#)
                .unwrap();
        match r {
            Response::Error(e) => {
                assert_eq!(e.code, error_code::PANE_NOT_FOUND);
                assert_eq!(e.message, "no");
            }
            other => panic!("{other:?}"),
        }

        assert!(matches!(
            parse_response(r#"{"id":"c"}"#).unwrap_err(),
            WireError::NeitherResultNorError
        ));
        assert!(matches!(
            parse_response("nope").unwrap_err(),
            WireError::Json(_)
        ));
    }

    #[test]
    fn wire_pong_parses_with_and_without_capabilities() {
        let p: Pong = serde_json::from_str(
            r#"{"type":"pong","version":"0.8.2","protocol":21,"capabilities":null}"#,
        )
        .unwrap();
        assert_eq!(p.version, "0.8.2");
        assert_eq!(p.protocol, 21);
        assert_eq!(p.capabilities, None);
        let p: Pong = serde_json::from_str(
            r#"{"type":"pong","version":"0.9.0","protocol":22,"capabilities":{"x":1},"future":true}"#,
        )
        .unwrap();
        assert_eq!(p.protocol, 22);
        assert!(p.capabilities.is_some());
    }

    #[test]
    fn wire_agent_status_unknown_values_do_not_fail() {
        for (raw, want) in [
            ("idle", AgentStatus::Idle),
            ("working", AgentStatus::Working),
            ("blocked", AgentStatus::Blocked),
            ("done", AgentStatus::Done),
            ("unknown", AgentStatus::Unknown("unknown".into())),
            ("napping", AgentStatus::Unknown("napping".into())),
        ] {
            let parsed: AgentStatus = serde_json::from_value(Value::String(raw.into())).unwrap();
            assert_eq!(parsed, want);
            assert_eq!(parsed.to_string(), raw);
            assert_eq!(
                serde_json::to_value(&parsed).unwrap(),
                Value::String(raw.into())
            );
        }
    }

    #[test]
    fn wire_snapshot_fixture_parses_and_tolerates_unknown_fields() {
        let text = fixture("snapshot_two_panes.json");
        let result: SnapshotResult = serde_json::from_str(&text).unwrap();
        let s = result.snapshot;
        // Recorded from the published v0.8.2 asset, which answers protocol 20 (see guard.rs).
        assert_eq!(s.protocol, 20);
        assert_eq!(s.version, "0.8.2");
        assert_eq!(s.workspaces.len(), 1);
        assert_eq!(s.panes.len(), 2);
        assert_eq!(s.agents.len(), 1);
        assert!(s.panes.iter().any(|p| p.is_agent_bearing()));
        assert!(s.panes.iter().any(|p| !p.is_agent_bearing()));

        // Inject fields no herdr version has; the parse must still succeed.
        let mut v: Value = serde_json::from_str(&text).unwrap();
        v["snapshot"]["panes"][0]["brand_new_field"] = serde_json::json!({"nested": true});
        v["snapshot"]["workspaces"][0]["another"] = serde_json::json!(1);
        v["snapshot"]["extra_top_level"] = serde_json::json!([1, 2, 3]);
        let again: SnapshotResult = serde_json::from_value(v).unwrap();
        assert_eq!(again.snapshot.panes.len(), 2);
    }

    #[test]
    fn wire_lifecycle_burst_fixture_parses_both_envelope_shapes() {
        let text = fixture("lifecycle_burst.jsonl");
        let mut names = Vec::new();
        let mut saw_dotted = false;
        let mut saw_snake = false;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let event = Event::parse_line(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert!(
                !matches!(event, Event::Unknown { .. }),
                "unexpected Unknown for {line}"
            );
            match &event {
                Event::PaneAgentStatusChanged(e) => {
                    saw_dotted = true;
                    assert!(!e.pane_id.is_empty());
                }
                _ => saw_snake = true,
            }
            names.push(event.name().to_string());
        }
        assert!(saw_dotted, "fixture must include the dotted per-pane shape");
        assert!(
            saw_snake,
            "fixture must include snake_case lifecycle events"
        );
        for expected in [
            "workspace_created",
            "tab_focused",
            "pane_created",
            "pane_focused",
            "pane_agent_detected",
            "pane_updated",
            "pane_closed",
            "pane_exited",
            "workspace_focused",
            "worktree_created",
            "worktree_removed",
            "pane_moved",
            "workspace_closed",
            PANE_AGENT_STATUS_CHANGED,
        ] {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
    }

    #[test]
    fn wire_status_fixture_ends_in_done() {
        // Recorded per-pane lines (dotted, untagged), followed by the recorded tab_focused
        // lifecycle hint that `just probe-hello` uses to trigger the done → idle resync.
        let text = fixture("status_working_to_done.jsonl");
        let events: Vec<Event> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| Event::parse_line(l).unwrap())
            .collect();
        let statuses: Vec<AgentStatus> = events
            .iter()
            .filter_map(|e| match e {
                Event::PaneAgentStatusChanged(s) => Some(s.agent_status.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(statuses.first(), Some(&AgentStatus::Working));
        assert_eq!(statuses.last(), Some(&AgentStatus::Done));
        assert!(
            matches!(events.last(), Some(Event::TabFocused { tab_id, .. }) if tab_id == "w1:t1"),
            "{events:?}"
        );
        for e in &events {
            if let Event::PaneAgentStatusChanged(s) = e {
                assert_eq!(s.pane_id, "w1:p1");
            }
        }
    }

    #[test]
    fn wire_subscribe_failure_fixture_is_an_error_line() {
        let text = fixture("subscribe_failure.jsonl");
        let line = text.lines().find(|l| !l.trim().is_empty()).unwrap();
        let (_, r) = parse_response(line).unwrap();
        assert!(matches!(r, Response::Error(_)));
    }

    #[test]
    fn wire_unknown_event_becomes_unknown_variant() {
        let e = Event::parse_line(
            r#"{"event":"layout_updated","data":{"type":"layout_updated","layout":{}}}"#,
        )
        .unwrap();
        assert!(matches!(e, Event::Unknown { ref event, .. } if event == "layout_updated"));
        let e = Event::parse_line(r#"{"event":"something.brand_new","data":{"x":1}}"#).unwrap();
        assert_eq!(e.name(), "something.brand_new");
        assert_eq!(e.pane_id(), None);
    }

    #[test]
    fn wire_known_event_with_malformed_data_is_an_error_not_a_panic() {
        let err = Event::parse_line(r#"{"event":"pane_created","data":{"type":"pane_created"}}"#)
            .unwrap_err();
        assert!(matches!(err, WireError::EventData { ref event, .. } if event == "pane_created"));
    }

    #[test]
    fn wire_events_never_carry_ids_we_rely_on() {
        // An id on an event line is ignored, not correlated.
        let e = Event::parse_line(r#"{"id":"sub_1","event":"pane_focused","data":{"type":"pane_focused","pane_id":"w:p1","workspace_id":"w"}}"#)
            .unwrap();
        assert_eq!(e.pane_id(), Some("w:p1"));
    }

    #[test]
    fn wire_lifecycle_subscription_set_matches_spec_5_4() {
        let subs = lifecycle_subscriptions();
        assert_eq!(subs.len(), 15);
        assert!(subs.iter().all(|s| s.pane_id.is_none()));
        let json = serde_json::to_value(&EventsSubscribeParams {
            subscriptions: subs,
        })
        .unwrap();
        assert_eq!(
            json["subscriptions"][0],
            serde_json::json!({"type": "workspace.created"})
        );
        let per_pane =
            serde_json::to_value(Subscription::pane_agent_status_changed("w:p1")).unwrap();
        assert_eq!(
            per_pane,
            serde_json::json!({"type": "pane.agent_status_changed", "pane_id": "w:p1"})
        );
    }

    #[test]
    fn wire_pane_info_agent_label_prefers_display_agent() {
        let mut pane: PaneInfo = serde_json::from_value(serde_json::json!({
            "pane_id": "w:p1", "terminal_id": "t", "workspace_id": "w", "tab_id": "w:t1",
            "focused": false, "agent_status": "idle", "revision": 0
        }))
        .unwrap();
        assert_eq!(pane.agent_label(), None);
        assert!(!pane.is_agent_bearing());
        pane.agent = Some("claude".into());
        assert_eq!(pane.agent_label(), Some("claude"));
        assert!(pane.is_agent_bearing());
        pane.display_agent = Some("Claude Code".into());
        assert_eq!(pane.agent_label(), Some("Claude Code"));
    }
}
