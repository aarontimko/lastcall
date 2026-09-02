# Provenance: snapshot_two_panes.json

Hand-written from the herdr v0.8.2 schema (`docs/next/api/herdr-api.schema.json`, commit `5158ada`):
`SessionSnapshot` at line 9840, `WorkspaceInfo` 1071, `WorkspaceWorktreeInfo` 1136, `TabInfo` 1032,
`PaneInfo` 760, `AgentInfo` 6139, the `session_snapshot` result variant at 8687. Ids follow herdr's
public id shape (`<workspace_id>:p<n>` / `<workspace_id>:t<n>`, `src/workspace.rs:145-151`).

Shape: one workspace with two tabs; pane `ws_demo1:p1` is agent-bearing (`agent: "demo"`,
`working`) in the non-active tab; pane `ws_demo1:p2` is a bare shell in the active tab.

Recording status: the real-herdr integration test (`just herdr-record`) writes a recorded
snapshot to `target/herdr-recordings/`; see `recorded/` alongside this file for the captured
lines from the pinned binary once recorded.
