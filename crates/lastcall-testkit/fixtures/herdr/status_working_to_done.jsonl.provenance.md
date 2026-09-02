# Provenance: status_working_to_done.jsonl

**Recorded from the real pinned herdr (v0.8.2 release asset)** by
`crates/lastcall-engine/tests/test_integration_herdr_real.rs` via `just herdr-record`, assembled
by `just fixtures-sync`:

- lines 1–2: `recorded/status_working_to_done.jsonl` verbatim — the dotted, untagged
  `pane.agent_status_changed` lines a per-pane subscription for `w1:p1` emitted for
  `pane.report_agent --state working` then `--state idle` while `w1:t1` was **not** the active
  tab. herdr derives `done` from that completion (`src/app/api_helpers.rs:96-104`:
  `(Idle, seen=false) => Done`); the reporting enum has no `done`.
- line 3: the recorded snake_case `tab_focused` for `w1:t1` from `recorded/lifecycle.jsonl`
  (the `tab.focus` that flips done → idle). The mock example routes it to the lifecycle stream
  (`ScriptedEvent::route`), so `just probe-hello` shows both `working → done` (per-pane
  stream) and `done → idle` (focus-driven snapshot resync against
  `snapshot_two_panes_after_focus.json`).

Schema cross-check: `PaneAgentStatusChangedEvent` line 5936 (the release omits `title`,
`display_agent`, `state_labels` when absent); `EventEnvelope` line 1220.
