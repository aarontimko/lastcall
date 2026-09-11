# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the versions
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). A version's section here is
exactly what its GitHub release notes carry, so it is written for the people installing the
binary rather than for the commit log.

## 0.1.0

The first release. lastcall watches every git repository under your working directory and
shows what changed since you last looked, whoever or whatever made the change, so that a
pile of agent edits can be read and answered instead of merely merged.

### The review ledger

- Every repository under a configured parent directory is watched, plus optional draft
  directories: a folder that is not a git repository, reviewed as a root of its own.
- What you have already seen is recorded in a private object store outside your repository.
  Nothing is ever written inside the repositories being watched, and an agent's commit moves
  `HEAD` without moving your baseline.
- The pile survives restarts: relaunching on the same state directory picks up where you
  stopped.

### The terminal UI

- `lastcall` (or `lastcall tui`) opens two panes: every root with its pending files on the
  left, the selected file's diff on the right. Counts change on screen within about a second
  of an edit in another terminal, and `--poll <seconds>` covers filesystem events that are
  late or missing.
- Keyboard and mouse: `↑↓`/`jk`, `enter`, `n`/`p` for hunks, `f` for full paths, `o` for
  `org/repo`, `t` to hide repositories with nothing pending, `r` to rescan, `?` for every
  key, `q` to quit. Click a row or a hunk header, drag the divider, scroll with the wheel.
- Lockfiles, binaries and very large files are one collapsed row instead of a wall of hunks.
  `e` expands the ones worth reading.

### Four answers to a change

- **Accept**: `a` takes the hunk under the cursor, `A` the file, `ctrl-a` everything across
  every repository, with a confirmation above ten files. An accept is compare-and-swap
  against what was on screen: a file that changed underneath is refused, not overwritten.
- **Restore**: `u` puts a hunk back the way it was, `shift-u` the whole file, both asking
  first when the result is a deletion. Restoring writes the working tree only, and refuses a
  file that moved.
- **Flag**: `m` opens a note on a hunk or a file and produces a paste-ready message with the
  note and the diff. With a herdr session it goes to the agent's pane; standalone it is
  written to the state directory.
- **Edit**: `i` opens lastcall's own editor, `shift-i` hands the file to your `$EDITOR` and
  takes the terminal back when it exits. `v` and `y` copy diff lines to the clipboard of the
  machine you are sitting at, even over ssh.

### The herdr overlay

When a [herdr](https://github.com/herdrdev/herdr) session is running, lastcall finds it,
shows each repository's agent status, narrows to the repositories the session is actually
working in (`w` toggles), and sends a flag straight into the right agent's pane. With no
session it runs standalone and says so. A herdr that speaks a protocol lastcall does not know
is a one-line notice, never a failure.

### Headless commands

- `lastcall status`, `lastcall status --json` (a stable report, `status_version` 1), and
  `lastcall status --root <path>` for one root.
- `lastcall watch` prints one line per event.
- `lastcall config` prints the effective configuration and any notices.

### Keeping it current

- `lastcall update` replaces the binary with the newest release after checking its SHA-256
  against the release's `SHA256SUMS`, and refuses a binary a package manager owns.
- `lastcall update --check` only answers the question.
- The UI asks once a day in the background, after the first screen is drawn, and shows
  `↑ <version>` in the header when a newer release exists. `check = false` under `[update]`
  in `config.toml` turns that off.

### Platforms

macOS on Apple silicon and Intel, Linux on arm64 and x86_64. The Linux binaries need glibc
2.35 or newer. Installation, checksums and attestation:
[`docs/install.md`](docs/install.md).
