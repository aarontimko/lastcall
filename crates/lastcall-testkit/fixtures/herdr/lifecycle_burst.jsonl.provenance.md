# Provenance: lifecycle_burst.jsonl

**Hand-written from the herdr v0.8.2 schema** (`docs/next/api/herdr-api.schema.json`, commit
`5158ada`): the `EventEnvelope` (line 1220) with `EventData` variants (lines 84–760:
`workspace_created`, `workspace_focused`, `tab_focused`, `pane_created`, `pane_focused`,
`pane_agent_detected`, `pane_updated`, `worktree_created`, `pane_moved`, `pane_exited`,
`pane_closed`, `worktree_removed`, `workspace_closed`) and the untagged
`PaneAgentStatusChangedEvent` (line 5936).

Why hand-written: it deliberately covers every lifecycle kind the client branches on — the
worktree events need a git repository inside the spawned herdr, and `pane_moved` /
`workspace_closed` are not driven by the integration test — and it mixes both envelope
shapes on one stream on purpose (spec §5.5). Its ids (`ws_demo2`, `ws_demo3`) are
intentionally foreign to `snapshot_two_panes.json`, so a client test can prove a dotted
status line on the *lifecycle* stream never becomes the status of record.

Shape reference: `recorded/lifecycle.jsonl` holds the real lines the release emits for
`workspace_created`, `workspace_focused`, `pane_created`, `pane_focused`, `tab_focused`,
`pane_agent_detected`, and `pane_closed`; field sets here match those (the release omits
absent optionals rather than sending `null`; both parse identically).
