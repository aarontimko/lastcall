# recorded/

Raw wire captures from the **pinned herdr release** (`just herdr-fetch`, v0.8.2 asset,
protocol 20), written by `crates/lastcall-engine/tests/test_integration_herdr_real.rs` when
`just herdr-record` runs it with `LASTCALL_RECORD_DIR` set. `just fixtures-sync` derives the
named fixtures in the parent directory from these files; each `<name>.provenance.md` there
states the rule.

| file | what |
|---|---|
| `snapshot_bootstrap.raw.json` | the raw `session.snapshot` response line right after bootstrap (one pane, no agent) |
| `snapshot_two_panes.json` | `result` of `session.snapshot` with p1 `working` in the non-active tab, p2 a bare shell |
| `snapshot_two_panes_after_focus.json` | same after `tab.focus` flipped p1 done → idle |
| `status_working_to_done.jsonl` | the per-pane `pane.agent_status_changed` lines: working, done |
| `lifecycle.jsonl` | every line the lifecycle subscription produced during the test |
| `subscribe_failure.jsonl` | the one error line for a bogus subscription set |

Re-recording changes the temp-dir cwd and terminal ids; that churn is expected.
