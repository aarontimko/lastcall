# Provenance: lifecycle_burst.jsonl

Hand-written from the herdr v0.8.2 schema (`docs/next/api/herdr-api.schema.json`, commit `5158ada`):
the `EventEnvelope` (line 1220) with `EventData` variants (lines 84-760: `workspace_created`,
`workspace_focused`, `tab_focused`, `pane_created`, `pane_focused`, `pane_agent_detected`,
`pane_updated`, `worktree_created`, `pane_moved`, `pane_exited`, `pane_closed`,
`worktree_removed`, `workspace_closed`) and the untagged `PaneAgentStatusChangedEvent`
(line 5936) as emitted by a per-pane `pane.agent_status_changed` subscription.

Both envelope shapes are present on purpose (spec §5.5): snake_case lifecycle events with a
tagged `data.type`, and the dotted `pane.agent_status_changed` with untagged data. No line
carries an `id`.

Recording status: `just herdr-record` captures real lifecycle lines from the pinned binary
into `target/herdr-recordings/`; see `recorded/` alongside this file once recorded.
