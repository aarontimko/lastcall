# hello-herdr: the Gate 1 sponsor demo

`lastcall hello-herdr` connects to a herdr session, prints the ping result and the compat
verdict, bootstraps a snapshot, and then streams what the client sees until Ctrl-C:

- every lifecycle event, one line each (`event: event=pane_focused ws=w1 pane=w1:p1`);
- every agent status transition as `[<pane_id>] <agent> working → done`;
- every resync (`resync: snapshot`, `resync: pane.get <id>`);
- connection state (`connected:`, `disconnected:`, `standalone:`, `status: subscribed <pane>`).

It is the human-visible face of the Phase 1 client: events are hints, snapshots are truth
(spec §2 invariant 9), per-pane status subscriptions (§5.4), and the focus-driven resync that
catches herdr's silent `done → idle` flip (§5.7).

## The recipe (one command plus three steps)

Build once: `just build` (dev) or `just cargo build --release` (then use
`target/release/lastcall`). In the transcript below `lastcall` means whichever you built.

**Command.** Inside a herdr session, split a pane and run:

```sh
lastcall hello-herdr
```

Discovery inside a herdr pane uses `HERDR_SOCKET_PATH`, so no flags are needed. Expected
first lines:

```
lastcall hello-herdr (events are hints; snapshots are truth)
session: /Users/you/.config/herdr/herdr.sock (EnvSocketPath)
ping: herdr 0.9.0 protocol 22 (supported: 20/21/22)
compat: ok
connected: herdr 0.9.0 protocol 22
workspaces (1): ...
panes (2): ...
agents (0):
streaming (ctrl-c to stop)...
```

**Step 1 — make an agent appear in another tab.** Open a second tab (call it tab B) with a
plain shell and note its pane id (`echo $HERDR_PANE_ID`, or `herdr pane list`). Either start a
real agent there, or fake one with herdr's own CLI:

```sh
herdr pane report-agent "$HERDR_PANE_ID" --source demo --agent demo --state working
```

hello-herdr (in tab A) prints `event: event=pane_agent_detected ...`, `status: subscribed
<B>`, and `[<B>] demo unknown → working` (a bare shell pane reports `unknown` on the released 0.8.2; a pane with no prior report at all shows `(none)`) — the pane is now
agent-bearing and has its own status subscription.

**Step 2 — complete the work while tab B is *not* focused.** Switch to tab A (hello-herdr's
tab), and from the other pane there report completion for tab B's pane:

```sh
herdr pane report-agent <B_pane_id> --source demo --agent demo --state idle
```

Expected in hello-herdr: **`[<B>] demo working → done`**. There is no `done` in the reporting
enum (`idle | working | blocked | unknown`); herdr derives `done` from a completion that
happened in a tab you were not looking at (`src/app/api_helpers.rs:96-104`). The real-herdr
integration test (`just test-integration-herdr`) asserts this derivation on the pinned
binary.

**Step 3 — focus tab B.** Switch to tab B. Expected within the fallback interval plus the coalesce window (30.5 s; in
practice within a second): **`[<B>] demo done → idle`**. herdr emits *no* global event for the
flip itself, so hello-herdr can learn it two ways, and the transcript tells you which:

- if the line is preceded by `resync: snapshot` (triggered by the `tab_focused` /
  `workspace_focused` events), the focus resync produced it;
- if the transition line arrives without a preceding `resync:` line, the per-pane status
  stream produced it (herdr's per-pane subscription polls and diffs internally, §5.4).

Paste the transcript into the PR; that is the `[sponsor]` gate item. Ctrl-C to stop.

## Notes

- `herdr api` has only `snapshot` and `schema` subcommands; `pane.report_agent` is reached
  through `herdr pane report-agent`, never `herdr api pane.report_agent`.
- The published v0.8.2 asset answers **protocol 20**; the spec's "protocol 21" is herdr master
  after the release; the pinned v0.9.0 asset answers **22**. All three are supported
  (`guard::SUPPORTED_PROTOCOLS`).
- Standalone outcomes print a notice and exit 1: no session found, several live sessions with
  no `herdr.session` pin in `config.toml`, or a protocol mismatch.

## The automated proxy: `just probe-hello`

Without a real herdr, `just probe-hello` builds the release binary, starts the mock server
example with the **recorded** fixtures (`snapshot_two_panes.json`,
`status_working_to_done.jsonl`, and `snapshot_two_panes_after_focus.json` swapped in after the
status script plays), and runs
`target/release/lastcall hello-herdr --socket <mock> --exit-after 5`. The output shows the
same two lines the sponsor sees — `[w1:p1] demo working → done` from the per-pane stream, then
`resync: snapshot` and `[w1:p1] demo done → idle` from the focus resync — through the exact code
paths a real session exercises.

`--socket` also makes the command exit on `disconnected:` instead of reconnecting, and
`--exit-after <secs>` ends the stream cleanly (exit 0), so the probe terminates on its own.

Exit codes: 0 after `--exit-after`, Ctrl-C, or (with `--socket`) a peer disconnect — the
session ended, nothing was wrong on our side; 1 for every standalone outcome (no session,
ambiguous sessions, ping failure, protocol mismatch, unreadable snapshot); 2 for a config
error.
