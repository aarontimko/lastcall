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
sees; see [`docs/dev/hello-herdr.md`](docs/dev/hello-herdr.md)). Phase 2 adds the headless
review engine behind `lastcall status` and `lastcall watch`. Phase 3 adds the terminal UI:
bare `lastcall` (or `lastcall tui`) shows every root's pile and updates it live.

## Try it

From inside any git repository (or a directory holding several):

```sh
just cargo build --release -p lastcall
cd ~/src/some-repo
~/path/to/lastcall/target/release/lastcall                 # the TUI: what changed since you last looked
```

The left pane lists each repo (branch, file count) and its pending files with `+added
−removed` counts; the right pane is the selected file's diff. `↑↓`/`jk` move, `enter` opens
a diff, `n`/`p` step hunks, `f` shows full paths, `o` shows `org/repo`, `r` rescans, `?`
lists every key, `q` quits; the mouse works too (click a row or a hunk header, drag the
divider, wheel to scroll). Edit a file in another terminal and its counts change on screen
within about a second. It is read-only in this phase: nothing is accepted, flagged or
restored yet — Phase 4 adds accepting hunks, files and whole repos, which is what shrinks
the pile. `lastcall tui --poll 2` polls every 2 s if filesystem events are late or missing.
Keys are rebindable in `config.toml`, one spec or a list per action (the full grammar and
table: [`docs/dev/tui.md`](docs/dev/tui.md)):

```toml
[keys]
quit = "ctrl-q"
hunk_next = ["n", "ctrl-n"]
```

The headless commands:

```sh
~/path/to/lastcall/target/release/lastcall status          # first sight: nothing pending
echo hi >> README.md
~/path/to/lastcall/target/release/lastcall status          # README.md, 1 hunk
~/path/to/lastcall/target/release/lastcall status --json   # the stable status_version 1 report
~/path/to/lastcall/target/release/lastcall status --root .  # only this root (exit 1 if not a watched root)
~/path/to/lastcall/target/release/lastcall watch --exit-after 30   # one line per event
~/path/to/lastcall/target/release/lastcall watch --poll 2           # if events are late/missing
```

Without a config file the launch directory is the parent dir; with one, a launch directory
outside `parent_dirs` is watched ad hoc and a notice says so. State goes to `~/.local/state/lastcall` (override with `LASTCALL_STATE_DIR`). Nothing is written
to the repository itself. How it works and how to look at its state:
[`docs/dev/engine.md`](docs/dev/engine.md). `just probe-status`, `just probe-watch` and
`just probe-tui` run the same against a generated three-root fixture.

License: MIT OR Apache-2.0.
