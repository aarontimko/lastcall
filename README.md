# lastcall

The last call before code ships: an agent-agnostic review ledger for the terminal. It watches every repo under your working directory, shows exactly what changed since you last looked, and lets you accept, flag, or restore it hunk by hunk, whichever agent or human made the edit.

Status: pre-release, under construction as a phased program. The design corpus and roadmap live in [`docs/spec/00-spec.md`](docs/spec/00-spec.md); the scenario test plan in [`docs/spec/01-scenarios.md`](docs/spec/01-scenarios.md). Contributor orientation: [`AGENTS.md`](AGENTS.md).

## Build

Requirements: [rustup](https://rustup.rs) (the repo pins `1.98.0` with `clippy` and `rustfmt`
in `rust-toolchain.toml`; rustup installs it on first use), [`just`](https://github.com/casey/just),
and `git`. `gh` is needed only for `just herdr-fetch` (the pinned herdr release used by the
integration tests).

```sh
just toolchain    # prints cargo 1.98.0 / rustc 1.98.0 once rustup is on PATH
just build        # debug build of every crate and target
just test-unit    # the canonical unit suite
just cargo build --release -p lastcall && ./target/release/lastcall --version
```

Phase 1 ships two subcommands: `lastcall config [--json]` (the effective configuration and any
notices) and `lastcall hello-herdr` (connect to a herdr session and stream what the client
sees; see [`docs/dev/hello-herdr.md`](docs/dev/hello-herdr.md)).

License: MIT OR Apache-2.0.
