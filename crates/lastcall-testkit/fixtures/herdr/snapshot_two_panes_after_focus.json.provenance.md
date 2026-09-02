# Provenance: snapshot_two_panes_after_focus.json

**Recorded from the real pinned herdr (v0.8.2 release asset)** by
`crates/lastcall-engine/tests/test_integration_herdr_real.rs` via `just herdr-record`, copied
verbatim by `just fixtures-sync` from `recorded/snapshot_two_panes_after_focus.json`.

Moment of capture: same session as `snapshot_two_panes.json`, after `pane.report_agent
--state idle` produced `done` on the per-pane stream and then `tab.focus w1:t1` cleared
herdr's seen flag: `w1:p1` is now `idle` (the silent done → idle flip of spec §5.7 — no
global event announces it; the integration test asserts the client surfaced it through the
`tab_focused`-driven snapshot resync).

Used by `just probe-hello`: the mock example serves `snapshot_two_panes.json` first and swaps to
this file once the status script has played (`--snapshot-after`), so the trailing
`tab_focused` line of `status_working_to_done.jsonl` makes the client's focus resync print
`done → idle` exactly as a real session would.
