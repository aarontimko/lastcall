# Provenance: status_working_to_done.jsonl

Hand-written from the herdr v0.8.2 schema (`docs/next/api/herdr-api.schema.json`, commit `5158ada`):
`PaneAgentStatusChangedEvent` at line 5936 (`pane_id`, `workspace_id`, `agent_status` required;
`agent`, `title`, `display_agent`, `state_labels` optional), the dotted untagged shape a per-pane
`pane.agent_status_changed` subscription emits.

The sequence `working` → `done` is what herdr derives for a completion in a non-active tab
(`src/app/api_helpers.rs:96-104`: `(Idle, seen=false) => Done`). The pane id matches
`snapshot_two_panes.json` so `just probe-hello` shows `[ws_demo1:p1] demo working → done`.

Recording status: `just herdr-record` captures the real per-pane lines produced by
`pane.report_agent` working → idle on a pane in a non-active tab; see `recorded/` once recorded.
