# AGENTS.md

lastcall: an agent-agnostic review ledger for the terminal (Rust). Read this first; follow the
links only when the task needs them.

## Golden path

Every gate command is a `just` target; run cargo only through `just` (the seed `justfile` puts
rustup's proxies first on `PATH`, so `just toolchain` must print `cargo 1.98.0`).

```sh
just toolchain              # cargo 1.98.0 / rustc 1.98.0 (rust-toolchain.toml)
just build                  # cargo build --workspace --all-targets
just lint                   # fmt --check, clippy -D warnings, engine without the herdr feature
just test-unit              # THE canonical suite: cargo test --workspace --lib --bins
just test-integration       # real git; real herdr only when LASTCALL_TEST_HERDR_BIN is set
just test-integration-herdr # just herdr-fetch (pinned v0.9.0) then the integration tier
just test-e2e               # the TUI: TestBackend snapshots + PTY scenes on the built binary
just test                   # the three tiers in order
just test-scenarios         # the docs/spec/01-scenarios.md suites (real git, temp fixtures)
just probe-status           # release binary: status + status --json over a three-root fixture
just probe-tui              # release binary: the interactive TUI over the same fixture (--poll 1)
just probe-tui-screen       # the PTY harness's transcript of the live-update demo (3 s)
just probe-tui-slow         # probe-tui with the scans stretched (scripts/slowgit): the launch hold at scale
just bench                  # the performance baseline on the release build (docs/dev/bench.md; ~4 min; not a gate)
just snapshots-update       # rewrite the TUI snapshots, then prove they pass; read every diff
just test-prepush           # what the pre-push hook runs: integration tier + 64-case proptests
just hooks-install          # pre-commit = just lint && just test-unit; pre-push = just test-prepush
just install-smoke          # docs/install.md's steps, executed in a container (Docker; exits 2 without it)
just release-prep 0.4.0     # the release branch and its one five-file commit; pushes nothing
```

Probes against the built binary: `just probe-config`, `just probe-hello`, `just probe-status`,
`just probe-watch` (the B1 notice arriving live). The herdr demo for a human: `just hello-herdr`
(see `docs/dev/hello-herdr.md`). Fixture upkeep: `just herdr-record` then `just fixtures-sync`;
`just golden-update` rewrites the `status --json` golden.

Crates: `crates/lastcall-engine` (library: config, herdr client; the review engine from
Phase 2; no terminal code), `crates/lastcall` (the binary and the Ratatui TUI under
`src/tui/`), `crates/lastcall-testkit` (test-only: mock herdr, PTY spawner, fixture repos,
the TUI PTY harness — dev-dependency only).

Rules that are enforced by grep or test: no `deny_unknown_fields` on herdr-facing types
(`crates/lastcall-engine/src/herdr`), `deny_unknown_fields` required on config types;
`std::env` only in `crates/lastcall-engine/src/env.rs`; `unsafe_code = "forbid"`; no direct
`libc`; unit tests in-module only, integration tests named `test_integration_*.rs`, e2e
`test_e2e_*.rs`.

### Design corpus: `docs/spec/00-spec.md`

The stamped spec (v1.0). §5 (the herdr surface) and §6 (our contracts) are **frozen**: do not
edit them; implement additive/no-op-when-absent and propose an amendment in the PR. Read §2
(invariants — events are hints, snapshots are truth), §8 (the phase gates), §10 (rulings).

### Scenario plan: `docs/spec/01-scenarios.md`

Every git/draft/herdr scenario the spec claims, as setup/action/expected pile. The shell
harness `scripts/harness/scenarios.sh` (`just harness`) executes the git-plumbing half; Phase 2
turns each into a named integration test.

### Phase kickoffs: `docs/spec/9N-phaseN-kickoff.md`

The operational spec for each phase (deliverables, gate checklist, traps). Read the current
phase's kickoff before building anything in that phase.

### The review engine: `docs/dev/engine.md`

How a root's state is laid out on disk and how to inspect it with `jq`/`GIT_DIR=store git`,
the two git runners and the read-only allowlist, the scan pipeline, the fail-open ladder, and
the `status --json` schema. Read it before touching `crates/lastcall-engine/src/{store,ledger,
index,scan,ops,engine,watcher}.rs`.

### hello-herdr demo: `docs/dev/hello-herdr.md`

The one-command-plus-three-steps sponsor recipe for the Gate 1 `[sponsor]` item, the
`just probe-hello` automated proxy, and the protocol-20/21 note.

### The TUI: `docs/dev/tui.md`

How the screen is a pure function of `App` (reducers `apply` / `handle` / `sync_roots`,
`render(&App) -> HitMap`, the loop and its quit order), the key table and the `[keys]`
grammar, the hit-map rule, how to add a widget with a snapshot, and the PTY harness. Read it
before touching `crates/lastcall/src/tui/` or either e2e test.

### Testing: `docs/dev/testing.md`

Tiers, file naming, how skips are reported, and the isolation rules — including the sacred
one: tests never touch the real herdr config or socket.

### The user documentation: `docs/install.md`, `docs/config.md`, `docs/review-loop.md`, `docs/herdr.md`

The public surface the README links to: installing, updating and uninstalling
(`install.md`); every config key, `[keys]` action, `[herdr]` and `[update]` key and
`LASTCALL_*` variable with its default (`config.md`); the launch-to-copy walkthrough with
one screen per step from the `just probe-tui` fixture (`review-loop.md`); what the overlay
adds, session discovery and the agent picker (`herdr.md`). Read the relevant page before
changing anything a user can see, and update it in the same commit. **House style for all
four, plus the README and `CHANGELOG.md`:** no em-dashes (commas, colons or a new sentence),
no email addresses, no home paths, and none of the program's own process vocabulary — that
stays in `docs/spec` and `docs/dev`.

### The install smoke check: `just install-smoke`

`docs/install.md`'s own steps, executed rather than trusted: `scripts/install-smoke.sh`
downloads the release asset, checks it against `SHA256SUMS` and runs a real `lastcall
update` inside a fresh container, on both Linux architectures by default so x86_64 never
ships untested. It takes two real release tags, or `--from-dir ./dist` for a rehearsal's
artifacts with no release needed. `--self-test` runs the version-arithmetic cases only, with
no Docker and no network; without Docker the recipe exits 2.

### Going public: `docs/dev/publishing.md`

The maintainer's one-day checklist for flipping the repository public: the settings and
branch protection to set (with the `gh` command for each), the labels the issue forms
need, putting `pull_request:` back into `ci.yml` and `scans.yml`, and the disclosure grep
that must return zero hits over both the tree and the history. Read it only when doing
that; nothing in it is needed to build or test.

### Operating the released project: `docs/dev/operations.md`

The standing handoff written when the last gate closed: how a release is cut (hand tags,
the crate version equal to the tag), the weekly jobs and what to do when each goes red,
the invariants that must not drift, what is deferred, and where the evidence for every
gate lives. Read it before cutting a release or answering a Dependabot, compat or scan
result. `just merge` and `just release-tag` merge and tag, so they are the maintainer's and
refuse inside an agent's shell; `just release-prep` is the half an agent may run.

### The performance baseline: `docs/dev/bench.md`

The `just bench` numbers (four scenarios at the ruled sizes: 100 clones / 4,000 rows, a
100,000-line diff, a 1,000-file burst under watch, a 50,000-file drop against the row cap)
with the machine block and how each metric is taken. Read it before touching the scan
pipeline or the watcher's timings, and re-run `just bench` after; targets are set at the
Phase 9 kickoff, not here.
