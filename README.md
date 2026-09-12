# lastcall

[![ci](https://github.com/aarontimko/lastcall/actions/workflows/ci.yml/badge.svg)](https://github.com/aarontimko/lastcall/actions/workflows/ci.yml)
[![license: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue)](#license)

![lastcall in a terminal: three repositories in a list, a diff beside them, a hunk flagged with a note, a file accepted, then the help overlay](docs/demo/lastcall.gif)

The last call before code ships: an agent-agnostic review ledger for the terminal. It
watches every repository under your working directory, shows exactly what changed since you
last looked, and lets you accept, flag, restore or fix it hunk by hunk, whichever agent or
human made the edit.

It is not a git client and it does not commit anything. It remembers what you have already
seen, which is the thing git cannot tell you when an agent has been rewriting a file all
morning.

## What it does

- **Watches a whole directory of repositories at once.** One screen, every repository,
  updated live as files change. Branch groups, `+added −removed` counts per file, and the
  selected file's diff beside the list.
- **Four answers per hunk.** `a` accepts it, `u` puts it back, `m` flags it with a note,
  `i` opens the file for editing right there. Accepting is what shrinks the list, and it
  survives a restart.
- **A pile that shrinks and stays shrunk.** What you accept is recorded outside your
  repositories, so a relaunch starts where you stopped, whatever the agent committed in
  between.
- **Notes that reach the agent.** A flag written next to a herdr agent is typed into that
  agent's pane, not into a file you will never open again.
- **Generated files collapse.** A lockfile, a binary or anything very large is one row to
  accept instead of a wall of hunks, and `e` expands one when you do want to read it.
- **Editing in place, or in your own editor.** `i` for lastcall's, `shift-i` for
  `$VISUAL`. A save writes the file and advances the baseline in one step.
- **Copy that works over ssh.** `v` and `y` put diff lines on the clipboard of the terminal
  you are actually sitting at.
- **Headless too.** `lastcall status`, `lastcall status --json` and `lastcall watch` read
  and report the same ledger with no screen involved.
- **Everything rebindable**, and an unknown config key is an error rather than a shrug.

## Install

```sh
cargo install --git https://github.com/aarontimko/lastcall --tag v0.1.0 lastcall
```

Prebuilt binaries for macOS and Linux, checksums, attestation, and `lastcall update`:
[`docs/install.md`](docs/install.md).

## Use it

```sh
cd ~/src            # a directory holding several repositories, or one repository
lastcall            # the screen; ? lists every key, q quits
```

- [`docs/review-loop.md`](docs/review-loop.md) walks it once: launch, read, accept,
  restore, flag, edit, copy, one screen per step.
- [`docs/config.md`](docs/config.md) is every configuration key, every keybinding and every
  environment variable.
- [`docs/herdr.md`](docs/herdr.md) is what the screen adds when it is running beside
  [herdr](https://github.com/herdrdev/herdr) agents.
- [`CHANGELOG.md`](CHANGELOG.md) is what changed.

`just probe-tui` builds the binary, generates a three-repository fixture in a temporary
directory and drops you into it, so you can try every key on this page without involving
any work of your own.

## Build from source

Requirements: [rustup](https://rustup.rs) (the repository pins `1.98.0` with `clippy` and
`rustfmt` in `rust-toolchain.toml`; rustup installs it on first use),
[`just`](https://github.com/casey/just), and `git`. `gh` is needed only for
`just herdr-fetch`, the pinned herdr release the integration tests run against.

```sh
just toolchain    # prints cargo 1.98.0 / rustc 1.98.0 once rustup is on PATH
just build        # debug build of every crate and target
just test-unit    # the canonical unit suite
just cargo build --release -p lastcall && ./target/release/lastcall --version
```

## Status

Working towards v0.1.0. The first release is tagged after this lands, so until the tag
exists the install line above has nothing to resolve and the build steps are the way in.
Expect keys, configuration keys and on-disk state to move between minor versions until 1.0,
with everything that moves written down in [`CHANGELOG.md`](CHANGELOG.md).

Bug reports and small fixes are welcome. A feature wants an issue before a pull request, so
the shape can be agreed before anyone writes it. The design record and roadmap live in
[`docs/spec/00-spec.md`](docs/spec/00-spec.md), the scenario test plan in
[`docs/spec/01-scenarios.md`](docs/spec/01-scenarios.md). Contributor orientation:
[`AGENTS.md`](AGENTS.md) and [`CONTRIBUTING.md`](CONTRIBUTING.md).

I built lastcall by directing coding agents against written specifications, with an
adversarial review before every merge, and the design record under `docs/spec` is the real
history of it.

## Contributing, security, issues

Setup, the test tiers, the commit convention and what a pull request is expected to carry:
[`CONTRIBUTING.md`](CONTRIBUTING.md). How to report a vulnerability (privately, never in a
public issue) and what the tool touches: [`SECURITY.md`](SECURITY.md). Bugs and feature
proposals go through the [issue chooser](https://github.com/aarontimko/lastcall/issues/new/choose);
questions and half-formed ideas belong in
[Discussions](https://github.com/aarontimko/lastcall/discussions).

## License

MIT OR Apache-2.0: [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
