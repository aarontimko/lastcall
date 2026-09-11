# lastcall

[![ci](https://github.com/aarontimko/lastcall/actions/workflows/ci.yml/badge.svg)](https://github.com/aarontimko/lastcall/actions/workflows/ci.yml)
[![license: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue)](#license)

The last call before code ships: an agent-agnostic review ledger for the terminal. It watches every repo under your working directory, shows exactly what changed since you last looked, and lets you accept, flag, or restore it hunk by hunk, whichever agent or human made the edit.

## Status

Pre-release, under construction, working towards v0.1.0. There is no
published release yet: build it from source with the steps below. Expect keys, config keys
and on-disk state to move before v0.1.0.

Bug reports and small fixes are welcome now. A feature wants an issue before a pull
request, so the shape can be agreed before anyone writes it. The design corpus and roadmap
live in [`docs/spec/00-spec.md`](docs/spec/00-spec.md); the scenario test plan in
[`docs/spec/01-scenarios.md`](docs/spec/01-scenarios.md). Contributor orientation:
[`AGENTS.md`](AGENTS.md) and [`CONTRIBUTING.md`](CONTRIBUTING.md).

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
bare `lastcall` (or `lastcall tui`) shows every root's pile and updates it live. Phase 4
adds accepting — hunk, file, group, repo, everything — which is what shrinks the pile and
survives a restart. Phase 5 adds the herdr overlay (below). Phase 6 adds draft dirs (a directory that is not a git repo, reviewed as
a root of its own) and collapsed rows: a lockfile, a binary or a very large file is one
accept, not a wall of hunks, and `e` expands one on demand when you do want to read it.
Phase 7 adds the other two answers a reviewer has: restoring a hunk or a file to what it
was, and flagging one with a note that goes straight to the agent that wrote it. Phase 8 adds
the fourth: editing in place — `i` for lastcall's own editor, `shift-i` for yours — plus
`v`/`y` to copy diff lines to the clipboard of the machine you are actually sitting at.

## Try it

From inside any git repository (or a directory holding several):

```sh
just cargo build --release -p lastcall
cd ~/src/some-repo
~/path/to/lastcall/target/release/lastcall                 # the TUI: what changed since you last looked
```

The left pane lists each repo (branch, file count) and its pending files with `+added
−removed` counts; the right pane is the selected file's diff. `↑↓`/`jk` move, `enter` opens
a diff, `n`/`p` step hunks, `f` shows full paths, `o` shows `org/repo`, `r` rescans, `i` and
`shift-i` open the file for editing, `v`/`y` copy diff lines, `?`
lists every key, `q` quits; the mouse works too (click a row or a hunk header, drag the
divider, wheel to scroll). Edit a file in another terminal and its counts change on screen
within about a second. Reviewing is accepting: `a` accepts the hunk under the cursor (or,
on a file row without the diff focused, the file; on a group or repo entry, all of it), `A`
accepts the selected file whole, `ctrl-a` (or a click on `[Accept All]`) accepts everything
listed across every repo — above ten files a modal asks first (`y`/`enter` confirm,
`n`/`esc` cancel). An accept is compare-and-swap against what was on screen: if the file
changed underneath, the status says `changed since rendered; not accepted` and the row
stays. A generated file — a lockfile, a binary, anything over `collapse_size_bytes` — is a
single `⊟` row instead of a diff: `a` or `A` accepts it whole, and `e` expands the
lockfile/large-file kind into real hunks (up to 2,000 lines; a binary is never expandable).
The pile shrinks to `nothing pending`, and a relaunch on the same state dir starts
from there, whatever the agent committed in between (an agent's commit moves HEAD, never
your baseline). `lastcall tui --poll 2` polls every 2 s if filesystem events are late or
missing.
### Put it back

Accepting is one of three answers. `u` puts the hunk under the cursor back to what it was
before the agent touched it; `shift-u` puts the whole file back and asks first. On a file
that was *added* since your baseline, putting it back means removing it, so both keys ask
and the question says `Delete f1?`. Restoring writes only the working tree — it is not an undo of an
accept, and a file that changed underneath is refused rather than overwritten.

### Flag and discuss

`m` opens a note on the hunk under the cursor (or on the file, from the repo list). Type,
press Enter — `Ctrl-J` for a newline, `Esc` to throw it away — and lastcall writes the flag
into the ledger and hands the agent a paste-ready message: the header line, your note, and
the hunk itself in a fenced diff block.

````text
lastcall flag · some-repo · src/parse.rs · hunk 2 of 3 · 2026-09-05T18:04:00Z
note: why is this unwrap safe? the caller can pass an empty slice

```diff
@@ -10,7 +10,8 @@
 let n = parse(s);
-    n.unwrap()
+    n.expect("parsed above")
 }
```
````

Where it goes depends on what is around. In a herdr session with exactly one agent under
that repo it is **staged** into that agent's input box — pasted, not submitted, so you press
Enter yourself. With several, lastcall asks which one rather than guessing. With no herdr
link, it is appended to `~/.local/state/lastcall/exports/<repo>/<date>.md`, ready to paste by
hand. The flag is written before any of this, so cancelling the send loses nothing: the row
keeps its `⚑` (`⚑2` for two notes) and the note reads beside the hunk it is about.
`shift-m` clears a file's flags.

### Edit in place

The fourth answer is to fix it yourself. `i` opens the file **inside lastcall**, right where
the diff pane was, with the caret on the current hunk's first changed line: the whole file is
there, every other pending hunk is marked `▎` in the gutter, and the hunk you came in on is
tinted so you can see which change you were reading. Type; `Ctrl-S` saves; `Esc` closes (and
asks first if you have unsaved changes). A save writes the file and advances your baseline in
one step, so the row disappears — you are never asked to review your own just-typed change.
If an agent wrote the file while you were typing, the save is refused, your buffer is kept
and the header turns red: `Esc`, then `i`, reloads.

`shift-i` hands the file to **your own editor** instead — `$VISUAL`, else `$EDITOR`, else
`vi` — at the same line, with the flags it wants (`vim +21 …`, `code --goto file:21 --wait`,
and so on for the editors it knows). lastcall gives up the terminal, waits, takes it back,
and asks whether to accept whatever you left. Use it for anything lastcall will not put in a
buffer: a binary, a very large file, or an editor you would rather not live without.

`v` starts a line selection in the diff and `↑↓` extend it; `y` copies it — or, with nothing
selected, the whole hunk under the cursor. A drag with the mouse does the same. The copy goes
out over OSC 52, which means it reaches the clipboard of the terminal you are sitting at even
when lastcall is running over ssh or inside tmux. Inside tmux that needs
`set -g set-clipboard on`; some terminals ship with OSC 52 turned off. Selections over 32 KiB
are refused rather than half-delivered.

Keys are rebindable in `config.toml`, one spec or a list per action (the full grammar and
table: [`docs/dev/tui.md`](docs/dev/tui.md)):

```toml
[keys]
quit = "ctrl-q"
hunk_next = ["n", "ctrl-n"]
```

Run inside a herdr pane and each repo row also carries its agents' status: `⚑` when an agent
finished in a tab you were not watching (that repo is listed even with nothing pending), a
red `●` blocked, a yellow `●` working. `d` acks the flag, `g` jumps to the agent, `w` toggles
the workspace scope. The `[herdr]` table:

```toml
[herdr]
mode = "auto"       # auto | on | off — "on" shows why a link failed in the header
session = "work"    # optional named-session pin
toast = true        # a desktop notification when a repo first goes ready (default true)
scope = "workspace" # workspace | all — which repos the overlay covers
```

The toast also needs herdr's own `[ui.toast] delivery = "herdr"`, which is `"off"` by
default; the details and the demo recipe are in [`docs/dev/tui.md`](docs/dev/tui.md).

### `config.toml`

`~/.config/lastcall/config.toml` (`XDG_CONFIG_HOME` honoured; `lastcall config` prints the
effective values and any notices). Every key is optional and an unknown key is an error:

| key | default | what it does |
|---|---|---|
| `parent_dirs` | `[]` (the launch cwd) | absolute paths whose git repos are watched |
| `draft_dirs` | `[]` | globs relative to a parent dir (`"_drafts/**"`, `"notes"`) or absolute paths: directories that are **not** git repos, reviewed as roots of their own |
| `draft_initial` | `"seen"` | what a draft root's first sight means — `seen` (start from zero, review only what changes after it) or `pending` (everything already there is pending) |
| `collapsed_globs` | the nine common lockfiles | paths shown as one collapsed row instead of hunks: `package-lock.json`, `yarn.lock`, `pnpm-lock.yaml`, `Cargo.lock`, `poetry.lock`, `uv.lock`, `Gemfile.lock`, `go.sum`, `composer.lock` |
| `collapse_size_bytes` | `524288` (512 KiB) | files **larger** than this collapse too; must be > 0. A file with a NUL byte in its first 8,000 is binary and collapses whatever this says |
| `ignore_globs` | `.git/**`, `node_modules/**`, `target/**`, `vendor/**`, `.venv/**` | scope the filesystem watcher only — an ignored path never wakes a scan, but the next scan still shows a tracked edit under it |
| `hide_empty_repos` | `false` | what the TUI's `t` toggle starts as: `false` shows **every** repo under a parent dir on the nav, pending or not; `true` opens with the repos that have nothing pending hidden (one carrying a herdr attention flag stays). `t` flips it for the session; the headless `status` commands never filtered by pending state and are unaffected |
| `[update]` `check` | `true` | once a day, after the first frame is drawn, the TUI asks the GitHub releases API in the background whether a newer version exists, and shows `↑ <version>` in the header if one does. Click it to read the whole sentence. `false` turns the background check off and is the only switch: there is no environment override. It never affects `lastcall update`, which you asked for by typing it |

```toml
parent_dirs = ["/Users/me/src"]
draft_dirs = ["_drafts/**", "notes"]
draft_initial = "seen"
hide_empty_repos = false
collapsed_globs = ["package-lock.json", "Cargo.lock", "*.min.js"]
collapse_size_bytes = 524288

[update]
check = true
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

## Contributing, security, issues

Setup, the test tiers, the commit convention and what a pull request is expected to carry:
[`CONTRIBUTING.md`](CONTRIBUTING.md). How to report a vulnerability (privately, never in a
public issue) and what the tool touches: [`SECURITY.md`](SECURITY.md). Bugs and feature
proposals go through the [issue chooser](https://github.com/aarontimko/lastcall/issues/new/choose);
questions and half-formed ideas belong in
[Discussions](https://github.com/aarontimko/lastcall/discussions).

## License

MIT OR Apache-2.0: [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
