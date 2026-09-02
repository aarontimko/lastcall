# Provenance: snapshot_two_panes.json

**Recorded from the real pinned herdr (v0.8.2 release asset, `just herdr-fetch`; answers
protocol 20)** by `crates/lastcall-engine/tests/test_integration_herdr_real.rs` via
`just herdr-record`, then copied verbatim by `just fixtures-sync` from
`recorded/snapshot_two_panes.json` (the `result` object of a raw `session.snapshot` response,
pretty-printed — whitespace only).

Moment of capture: after `workspace.create` (pane `w1:p1` in tab `w1:t1`), `tab.create` with
focus (pane `w1:p2` in tab `w1:t2`, now the active tab), and `pane.report_agent w1:p1
--source lastcall-test --agent demo --state working`. So `w1:p1` is agent-bearing and
`working` in the **non-active** tab; `w1:p2` is a bare shell (`agent_status: "unknown"`, no
`agent`) in the active tab. `agents[]` carries the one agent.

Schema cross-check (herdr `docs/next/api/herdr-api.schema.json` @ `5158ada`): `SessionSnapshot`
line 9840, `PaneInfo` 760, `WorkspaceInfo` 1071, `TabInfo` 1032, `AgentInfo` 6139. Fields the
release omits when absent (`label`, `agent`, `display_agent`, `title`, `state_labels`,
`tokens`) are absent here too; the `AgentInfo` entries carry `tab_id`, `terminal_id`,
`state_change_seq` that our type ignores (no `deny_unknown_fields`).

Re-record with `just herdr-record` (the cwd and terminal ids change every run).
