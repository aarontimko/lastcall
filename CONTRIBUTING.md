# Contributing to lastcall

lastcall has one maintainer, [@aarontimko](https://github.com/aarontimko). The project is
pre-release and working towards v0.1.0, so the surface still moves. Bug reports and small
fixes are welcome now; a feature needs an issue first so we can agree on the shape before
you write it.

## Setup

You need four things:

- [rustup](https://rustup.rs). The toolchain is pinned in `rust-toolchain.toml` (`1.98.0`
  with `clippy` and `rustfmt`); rustup installs it the first time you build.
- [`just`](https://github.com/casey/just). Every gate command is a `just` target, and cargo
  is run through `just` so CI and your machine cannot drift.
- `git`.
- `gh`, only for `just herdr-fetch`, which downloads the pinned herdr release the
  real-herdr integration tests run against.

```sh
just toolchain      # must print cargo 1.98.0 / rustc 1.98.0
just build
just hooks-install  # see "Hooks" below
```

## Tests

The tiers, exactly as the `justfile` and [`docs/dev/testing.md`](docs/dev/testing.md) name
them:

| command | what it runs |
|---|---|
| `just test-unit` | the canonical suite: in-module `#[cfg(test)]` only, `cargo test --workspace --lib --bins` |
| `just test-integration` | real git; the real-herdr subset only when `LASTCALL_TEST_HERDR_BIN` is set |
| `just test-integration-herdr` | `just herdr-fetch` (the pinned release), then the integration tier against it |
| `just test-e2e` | the TUI: `TestBackend` snapshot scenes and PTY scenes against the built binary |
| `just test-prepush` | the integration tier plus the proptests at 64 cases |
| `just lint` | `cargo fmt --check`, `cargo clippy -D warnings`, the engine without the herdr feature, and the no-direct-`libc` greps |

Before opening a PR, run `just lint` and `just test-prepush`. Both must be green.

`just test-unit` is a ratchet: its count never shrinks across commits. If your change
removes a test, say why in the PR.

## Hooks

`just hooks-install` points `core.hooksPath` at the committed `.githooks/`. After that:

- **pre-commit** runs `just lint` and `just test-unit`. A formatting or clippy failure
  aborts the commit, so check `git log` if a commit seems to have vanished.
- **pre-push** runs `just test-prepush`.

Install them. They are the same commands CI runs, so a green commit locally is a green
commit in CI.

## Commits

[Conventional Commits](https://www.conventionalcommits.org): `feat:`, `fix:`, `docs:`,
`test:`, `ci:`, `refactor:`, `chore:`. Add a scope in parentheses when it helps
(`fix(tui):`, `docs(dev):`). The subject line says what changed and why; use the body when
the why is not obvious from the diff.

```
fix(engine): a case-only rename is one row, not an add plus a delete

APFS reports both paths, so the pairing pass matched neither side.
```

## Pull requests

- Small. One thing per PR. Two unrelated fixes are two PRs.
- Behaviour changes come with tests.
- Docs change in the same PR as the code, not a follow-up.
- CI green.
- One maintainer review before merge.
- The PR title is a conventional-commit subject.

Drive-by fixes (a typo, a broken link, a dead comment) need no issue: just open the PR.
Features do: open an issue describing the problem first.

## Triage

New issues get `needs-triage`. Within a week each one is labelled, asked for a
reproduction, or closed with a reason. A closed issue always says why it was closed.

Never report a security problem in a public issue. See [SECURITY.md](SECURITY.md).

## Where things live

- [`AGENTS.md`](AGENTS.md): the golden path. Every gate command, what each crate holds, and
  the rules that are enforced by grep or test. Read it before your first change.
- [`docs/dev/`](docs/dev/): the runbooks. `engine.md` (state on disk, the scan pipeline,
  the `status --json` schema), `tui.md` (the screen as a pure function of `App`, the key
  table, how to add a widget with a snapshot), `testing.md` (tiers, naming, isolation
  rules), `bench.md` (the performance baseline), `publishing.md` (maintainer only).
- [`docs/spec/00-spec.md`](docs/spec/00-spec.md): the design record. Sections 5 and 6 are
  frozen; implement additively and propose an amendment in the PR rather than editing them.

## Code of conduct

By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md).
