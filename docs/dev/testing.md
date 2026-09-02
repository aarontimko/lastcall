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
| e2e | `just test-e2e` = `cargo test --workspace --test 'test_e2e_*'` | placeholder until Phase 3 (Ratatui `TestBackend`) / Phase 9 (PTY) | — |

`just test` runs the three in order. **`just test-unit` is the canonical suite**; its count is
the ratchet floor from Phase 2 on (Phase 1 close: 85 engine + 16 testkit + 0 binary = 101, the
Phase 2 floor; Phase 2 close: 162 engine + 16 testkit + 0 binary = 178, the Phase 3 floor; one engine test self-skips where filesystem events are not delivered).
The suite never shrinks across commits.

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
- No sleeps longer than 50 ms anywhere in the unit tier.

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
