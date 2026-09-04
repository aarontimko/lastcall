# Testing

**The sacred rule, in one line: tests never touch the real herdr config or socket — every test
injects `Env`.** The only file in the engine that reads `std::env` is
`crates/lastcall-engine/src/env.rs` (`rg -n 'std::env::var|home_dir\(' crates/lastcall-engine/src`
must match only that file); unit tests build an `Env::empty(..)` and add exactly the variables
they mean to test. The real-herdr spawner removes every inherited `HERDR_*` variable from the
child and never falls back to a `herdr` on `PATH`.

## Tiers

| tier | command | what runs | where |
|---|---|---|---|
| unit | `just test-unit` = `cargo test --workspace --lib --bins` | in-module `#[cfg(test)]` only | everywhere, incl. macOS CI |
| integration | `just test-integration` = `cargo test --workspace --test 'test_integration_*'` | real git; the pinned herdr when `LASTCALL_TEST_HERDR_BIN` is set | CI via `just test-integration-herdr`; Linux blocking, macOS best-effort |
| e2e | `just test-e2e` = `cargo test --workspace --test 'test_e2e_*'` | the TUI: twenty-three `TestBackend` snapshot scenes and seven PTY scenes against the built binary (`docs/dev/tui.md`) | everywhere; the PTY file skips with a visible reason only where no pseudo-terminal can be opened |
| bench | `just bench` = release build, then `cargo test --release -p lastcall --test test_bench -- --ignored --nocapture --test-threads=1` | the four `#[ignore]`d baseline scenarios (`docs/dev/bench.md`); **not a gate** in Phase 4 — targets are set at the Phase 9 kickoff | by hand, on the machine named in `bench.md` |
| pre-push | `just test-prepush` = `just test-integration`, then `PROPTEST_CASES=64 cargo test -p lastcall-engine --lib proptests` | the integration tier plus the two store-backed proptests at 64 cases (the unit tier runs them at 8) | the pre-push hook; by hand before a push from a machine without the hook |

`just test` runs the three tiers in order. **`just test-unit` is the canonical suite**; its count is
the ratchet floor from Phase 2 on (Phase 1 close: 85 engine + 16 testkit + 0 binary = 101, the
Phase 2 floor; Phase 2 close: 166 engine + 16 testkit + 0 binary = 182, the Phase 3 floor; Phase 3 close: 168
engine + 19 testkit + 64 binary lib + 4 binary main = 255, the Phase 4 floor; Phase 4 engine
work (4a): 179 engine + 19 testkit + 64 binary lib + 4 binary main = 266; Phase 4 TUI work
(4b): 180 engine + 19 testkit + 95 binary lib + 4 binary main = 298; the rename-pairing fix:
183 engine = 301; the Phase 4 review fold: 183 engine + 19 testkit + 100 binary lib + 4
binary main = 306; Phase 4 e2e + bench work (4c): 184 engine + 21 testkit + 100 binary lib
+ 4 binary main = 309; the Phase 4 close-out review fold (the rss sampler tests): 184
engine + 23 testkit + 101 binary lib + 4 binary main = 312, the Phase 5 floor; the hunk
separator and the selected-header band: 102 binary lib = 313; the nav-pane hunk accept, the
focus arrows and the shift-drag note: 106 binary lib = 317).
The suite never shrinks across commits. One recorded exception: at the Phase 2 code review
the three filesystem-live watcher tests (up to 30 s waits, real FSEvents) left the unit tier
for `crates/lastcall-engine/tests/test_integration_watcher.rs` because they contradicted the
determinism rules below (162 → 159 engine), and the same review added six engine unit tests
(159 → 165). The pure routing/allowlist watcher tests stay in `watcher.rs`.

## Hooks (`just hooks-install`)

`core.hooksPath` is pointed at the committed `.githooks/`. **pre-commit** runs `just lint`
and `just test-unit` — exactly the gate's lint and unit commands, so every commit is green
under both and the unit tier must stay fast (a few seconds in the engine crate). **pre-push**
runs `just test-prepush`: the integration tier and the store-backed proptests at 64 cases,
the checks that are too slow for every commit but must be green before anything leaves the
machine. Both print what they are running and fail the commit or push on the first red step.
CI runs the same targets (`just lint`, `just test-unit` with `PROPTEST_CASES=64`,
`just test-integration-herdr`, `just test-e2e`).

## Scenario suites (`just test-scenarios`)

`crates/lastcall-engine/tests/test_integration_scenarios_{a..f}.rs` hold one
`scenario_<id>_<slug>` test per `docs/spec/01-scenarios.md` ID (A accepts, B history, C
upstream, D edge cases, E storage faults, F draft roots). Each builds a `FixtureRepo` (or a
parent dir of them) in a temp dir, opens the engine through `tests/common::Fresh` with first
sight done, mutates the worktree/history with real git, and asserts the pile in the
harness's format with `lastcall_testkit::assert_pile!` — the expected string of every
harness-covered (H) scenario is copied verbatim from `scripts/harness/scenarios.sh`. E1
re-executes the test binary as a child role and SIGKILLs it at each fault point
(`FaultPoint::AfterObjectWrite`, `AfterLedgerTmpWrite`); D4 (case-only rename) and D6 (sparse
checkout) self-skip with a printed reason when the filesystem or git cannot produce the
precondition. `just test-integration` runs them too.

`crates/lastcall-engine/tests/test_integration_accept_loop.rs` (Phase 4 kickoff deliverable
9(a)) drives the whole reviewer loop through `Engine::accept` — the TUI's path — against a
fixture "agent" that edits three files and commits behind the reviewer: hunk → file →
accept-all → drop the engine, agent commits the rest, reopen → one more edit shows exactly
that delta. Every step asserts the override map, then the folded seen tree (entries by oid)
and `seen_at.head_commit`, which the agent's commits never move.

## The `status --json` golden

`crates/lastcall/tests/test_integration_status_golden.rs` builds one temp parent dir with
`lastcall_testkit::fixture_parent` (repo A: an uncommitted edit plus an agent commit; repo B:
a fast-forward pull of two coworker files with one edited on top; a draft dir with one edit —
first sight of all three happens **before** those operations), runs the built binary
(`env!("CARGO_BIN_EXE_lastcall")`) with `LASTCALL_CONFIG` naming a config whose
`parent_dirs = [W]` and `LASTCALL_STATE_DIR` in a temp dir, replaces `W` with `<W>`, and
compares byte-for-byte to `crates/lastcall/tests/golden/status_multi_repo.json`. Commit oids
are stable because fixtures use fixed identities and dates. To update after an intentional
schema change: `just golden-update` (sets `LASTCALL_UPDATE_GOLDEN=1`), then review the diff
and commit the file.

## The e2e tier: snapshots and the PTY

Both files live in `crates/lastcall/tests/`; `docs/dev/tui.md` has the how-to.

`test_e2e_tui_snapshots.rs` renders twenty-three scenes (the sixteen Phase 3 ones and the
seven Phase 4 accept scenes, which drive the real `Engine::accept` from the reducer's own
`Effect::Accept` and feed `App::accepted`; `tui_accept_all_confirm` pins a second `_live`
frame) through `ratatui::backend::TestBackend`
from an `App` fed by a real engine over the shared `fixture_parent` (each scene builds its
own fixture and state dir under a temp dir) and pins each as two `insta` snapshots under
`crates/lastcall/tests/snapshots/`: `<scene>_frame` (the symbols, exactly as a 100×30 — or
the scene's own size — terminal would show them) and `<scene>_styles` (the non-default style
runs: `<row> <from>..<to> <fg> <bg> <modifiers>`, which is where an inverted hunk header or a
focused border is visible). A failing snapshot test prints insta's unified diff: `-` lines
are the committed frame, `+` lines the new one; a moved column or a changed count is a
real change, a temp path is a leak (frames must show basenames and root-relative paths
only). To accept an intentional change: `just snapshots-update`, read every rewritten
`.snap` in the diff, commit them with the code change. Never regenerate to make red go
green.

`test_e2e_tui_pty.rs` spawns the built binary (`env!("CARGO_BIN_EXE_lastcall")`) with `tui
--poll 1` inside a real pseudo-terminal (`lastcall_testkit::pty_tui`; `portable-pty` +
`vt100`) over a fresh fixture parent, with `HOME`, `LASTCALL_CONFIG` and
`LASTCALL_STATE_DIR` injected per scene, and asserts on the parsed screen: first frame,
live update after an edit (the clock starts after the write returns; the minimum of two
tries must be ≤ 1.75 s and both are printed), hunk keys, a mouse click, a resize, and the
restored terminal after `q` / Ctrl-C (the raw transcript must carry the mouse-off and
alternate-screen-off sequences and no log line). The Phase 4 scenes
(`pty_accept_loop_and_restart`, `pty_accept_refused_when_file_moves`) drive `a` / `A` /
`ctrl-a` + `y` against a fixture agent's edits, read the `ledger.json` files back after the
fold, and relaunch a **second process** on the same state dir to show the empty state. The
scenes are serialized (one mutex); the whole file is about 30 s. Timing lines go to `stderr().write_all` so they survive libtest's
capture — run it with `-- --nocapture` to see them. If the live-update assertion fails on a
loaded host, report the measured numbers; do not loosen the budget.

## Naming

- Unit tests live in-module (`#[cfg(test)] mod tests`) and nowhere else: a `tests/test_unit_*.rs`
  file would not be run by `--lib --bins` and would not be counted.
- Integration tests: `crates/<crate>/tests/test_integration_<topic>.rs`.
- End-to-end tests: `crates/<crate>/tests/test_e2e_<topic>.rs`.
- Cargo errors on a `--test` glob with zero matches, so each tier keeps at least one file.

## Determinism rules for the unit tier

- No network, no git repos except temp fixtures, no sockets except the in-test mock.
- The herdr client state machine is tested on the **in-memory** mock transport under
  `#[tokio::test(start_paused = true)]` with production timings (500 ms coalesce, 30 s
  fallback, 250 ms → 10 s backoff): a paused clock costs nothing. Never pause time over a real
  socket — the clock auto-advances whenever the runtime idles on the read and every timer
  fires "instantly".
- Transport tests use the socket mock with real time and timeouts ≤ 50 ms.
- No sleeps longer than 50 ms anywhere in the unit tier. Anything that waits on a real
  filesystem watch or a polling backstop is an integration test.
- Property tests over real git (`ops::tests::proptests`, Phase 4 kickoff deliverable 8:
  accept-all-then-edits and hunk/file accept interleavings) run through
  `proptest::test_runner::TestRunner` so one draft-root fixture per test is shared across
  cases, with `failure_persistence: None` (no `proptest-regressions/` files to commit).
  The case count is `crate::env::proptest_cases()`: **8 by default** (the unit tier, so
  the pre-commit hook stays fast) and `PROPTEST_CASES` when set — the pre-push hook and
  CI export 64. It is read explicitly (and in `env.rs`, the engine's one `std::env`
  reader) because an explicit `cases:` field in a `ProptestConfig` silently overrides the
  variable proptest would otherwise honour. Each case reuses the fixture's store and
  rewrites its file set; at ~13 ms per git spawn on the development machine the two tests
  take ~2.5 s and ~3.5 s at 8 cases (they were ~9 s and ~14 s at the original 32, the two
  slowest unit tests by far; the pair is ~28 s at 64). The pure `hunks` proptest stays at
  1000 cases.
- A test that guards against a hang (`engine_scan_returns_under_a_global_fsmonitor_config`)
  runs the engine on a thread and bounds it with `recv_timeout` (5 s) — the bound is a
  failure, never a wait the passing path takes.

## Not a gate: `test_perf_scan`

`crates/lastcall-engine/tests/test_perf_scan.rs` is `#[ignore]`d evidence, not a tier: 2,000
tracked files, an unreadable ledger (so every file is a row), and `PERF` lines with the
process-wide git spawn count (`lastcall_engine::git::spawn_count`) and wall time per scan,
then accept-all and a no-change scan that must hash nothing. Run it by hand with
`cargo test -p lastcall-engine --test test_perf_scan -- --ignored --nocapture` when touching
the scan pipeline; at the Phase 2 close a scan of everything-unseen was 16 git processes and
under 0.5 s (it was 2,018 processes and 25 s before blobs were fetched in one
`cat-file --batch`).

## Not a gate: `test_bench` (`just bench`)

`crates/lastcall/tests/test_bench.rs` is the Phase 4 performance baseline: four
`#[ignore]`d scenarios at the sizes ruled in `docs/spec/93-phase4-kickoff.md`, run against
the release build only (a debug build prints a SKIP line) and recorded in
`docs/dev/bench.md`. Each prints `BENCH <scenario> <metric>=<value>` lines; fixture
construction is outside every timed region; RSS is `ps -o rss=` sampled by the PTY harness
(`PtyCommand::sample_rss`, `PtyTui::peak_rss_kb`). No target is asserted in this phase
(S4's `rows_shown == cap` and `omitted == 50,000 − cap` are correctness assertions, not
budgets); the Phase 9 kickoff sets the targets against these numbers.

## How skips are reported

The real-herdr test needs the pinned binary. Without `LASTCALL_TEST_HERDR_BIN` it writes
`SKIP: LASTCALL_TEST_HERDR_BIN unset (run: just test-integration-herdr)` with
`stderr().write_all` (libtest swallows `eprintln!` of passing tests) and returns; `just
test-integration` prints the same skip at the shell level, so it is visible twice rather than
never. `just test-integration-herdr` fetches the pinned release (`gh release download`, the only
sanctioned network fetch besides cargo and rustup) and exports the variable.

## Isolation for the real herdr (`lastcall_testkit::herdr_spawn`)

Per spawn: `/tmp/lc-<pid>-<nanos>/` (never `$TMPDIR` — macOS caps Unix socket paths at 104
bytes; the path is asserted under 100), private `XDG_CONFIG_HOME`, `XDG_RUNTIME_DIR`, `HOME`,
explicit `HERDR_SOCKET_PATH`, `SHELL=/bin/sh`, `onboarding = false` written before spawning,
every inherited `HERDR_*` removed. Socket readiness is polled (`exists && connect`) every 25 ms
up to 5 s. Kill-on-drop and kill-on-panic through a PID registry with a matcher
(`ps -o comm= -p`) that refuses to kill anything it did not spawn. Safe wrappers only
(`unsafe_code = "forbid"`, no `libc`).

## Fixtures

`crates/lastcall-testkit/fixtures/herdr/`: each fixture has a `<name>.provenance.md` saying
whether it was recorded from the pinned binary (preferred) or hand-written from the schema,
and from which schema lines. `just herdr-record` re-records into `recorded/`;
`just fixtures-sync` derives the named fixtures. Fixture git repositories come from
`lastcall_testkit::fixture_repo::FixtureRepo` (deterministic identity, dates, and config; a
local bare `origin`; `coworker_push`), proven by `test_integration_fixture_repo.rs`.
`lastcall_testkit::engine` opens an engine over one (`open_engine`, `assert_pile!`, the
`KillAt` fault injector); `lastcall_testkit::fixture_parent` builds the golden's three-root
parent dir (also the `just probe-status` / `probe-watch` fixture via
`examples/fixture_parent.rs`). Every engine in a test gets its `Env` from
`FixtureRepo::engine_env` / `engine_env_for` (private `HOME`, null global git config,
`LASTCALL_STATE_DIR` in a temp dir): tests never touch `~/.config` or `~/.local/state`.
