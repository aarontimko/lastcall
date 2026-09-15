# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the versions
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). A version's section here is
exactly what its GitHub release notes carry, so it is written for the people installing the
binary rather than for the commit log.

## 0.2.0 - 2026-09-14

### First launch

- The first time you open the review screen, a small card names the keys you need and then
  gets out of the way. The frame underneath stays live, so the scan you launched keeps
  running while you read it, and it never opens on a window too small to read.
- The card after the keys asks how far down lastcall looks. One folder below the directory
  you launched in is what it has always done; two also reaches a clone or a worktree kept in
  a folder such as `worktrees/<name>`, and a worktree kept inside a repository you already
  watch. Choosing two writes `search_depth = 2` and the repositories appear on the screen
  behind the card, with no relaunch. The card names your config file for the settings that
  go further than that.
- Two more of the cards offer to remember a default: showing every repository instead of only
  the ones your herdr workspace is working in, and starting with the empty repositories hidden.
  Either choice takes effect at once and is written into your config file, which keeps its
  comments, its key order and its formatting. If there is no config file yet, one is created
  holding just that setting.
- It is shown once. `lastcall tui --tour` brings it back, and the help overlay says so.

### Undo

- `z` reverses the last accept in the selected repository, and again for the one before it,
  up to the last twenty. Nothing on disk moves: the files go back to pending with their
  flags intact, and the cursor lands on the first of them.
- Each repository has its own stack, kept beside the rest of what lastcall remembers, so an
  accept from this morning can still be undone tonight. Saving a file in the built-in editor
  is on the stack too, because saving marks the file reviewed.
- `lastcall status --json` reports the depth as `undo` per root.

### Snooze

- `s` sets a repository aside for a number of days, one by default. `shift-s` lists the ones
  you have set aside, each with the date it comes back, and `s` on one of those wakes it now.
- It is a view and nothing more: the repository is still watched, still scanned, and still
  reported by `lastcall status`. An agent asking for attention brings it back into the list
  on its own, and the deadline expires on screen without a relaunch.
- The bottom line keeps the count, beside the workspace scope's when both apply.
- `lastcall status --json` reports the deadline as `snoozed_until` per root.

### Moving around

- `Home` and `End` go to the first and the last entry of the list, and to the top and the
  end of the diff when that pane has the keys.
- `{` and `}` jump to the previous and the next repository's own row, stepping over its
  files rather than walking through them. Option with an arrow key does the same in
  terminals that send it, and every one of the four can be rebound in `[keys]`.

### Changed

- `shift-a` is now the key that accepts a whole entry from the list: a file, a branch group,
  or a whole repository from its row. `a` accepts the hunk under the cursor and nothing
  larger, and on a group or a repository row it says which key to use instead. A repository
  of ten files or fewer used to vanish on a lowercase `a` with no question at all, because
  the confirmation only asks above ten.
- The `parent_dirs` documentation said every repository under a parent directory is watched.
  It is the repositories directly under it. One a folder deeper, such as `worktrees/<name>`,
  is now reached with the new `search_depth` key (`1` to `4`, default `1`), which also lists
  a worktree kept inside a repository you already watch; a clone that lives somewhere else
  entirely still wants an entry of its own. The walk never enters a repository or a
  dependency folder, so `3` and `4` want a narrow parent directory. `search_depth` is new in
  this release: a 0.1.0 binary refuses a config file that has the line, so delete it before
  going back to that version.

## 0.1.0 - 2026-09-12

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

### Configuration

- `~/.config/lastcall/config.toml` (`LASTCALL_CONFIG` and `XDG_CONFIG_HOME` honoured), and
  no configuration at all is a supported way to run: the directory you launch in becomes the
  directory that is watched.
- What to watch (`parent_dirs`, `draft_dirs`, `draft_initial`), what to collapse
  (`collapsed_globs`, `collapse_size_bytes`), what the watcher ignores (`ignore_globs`), and
  what the `t` toggle starts as (`hide_empty_repos`).
- Every key is rebindable in `[keys]`, one spec or a list per action, with `ctrl-`, `alt-`
  and `shift-` prefixes. An entry replaces that action's defaults rather than adding to them.
- Every key is optional and **an unknown key is an error**, naming the key and the line, so
  a typo never silently does nothing. So are an unknown action, an unparsable key spec and
  two actions bound to the same key, all reported by `lastcall config` before you are in a
  full screen.
- The whole surface: [`docs/config.md`](docs/config.md).

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

### Documentation

[`docs/install.md`](docs/install.md) for getting the binary,
[`docs/review-loop.md`](docs/review-loop.md) for the walkthrough,
[`docs/config.md`](docs/config.md) for every key and environment variable, and
[`docs/herdr.md`](docs/herdr.md) for the overlay.

### Worth knowing in this release

- **The state directory only grows.** Nothing collects old baselines yet. It is safe to
  delete, whole or per repository; what you lose is the memory of what you have already
  seen. Deleting it never touches a watched repository.
- **Filesystem events are not available everywhere.** On some network filesystems and inside
  some containers they do not arrive, and `lastcall tui --poll <seconds>` is the fallback.
- **A restore replaces the file.** An editor holding that file open elsewhere is now looking
  at stale contents and will overwrite the restore if you save from it; reload the buffer.
- **A restore can refuse**, and says which file and why: one that changed since the screen
  drew it, one git is holding open in a merge conflict, and one whose bytes git's own filters
  do not reproduce exactly (a line-ending conversion or a clean filter that is not round trip
  safe).
- **`shift-i` blesses what is on disk.** Answering its confirmation accepts the file as it
  stands when your editor exits, which is not necessarily only what you typed.
- **Per-branch history is not kept.** What you have seen is recorded per repository, not per
  branch, so switching branches re-presents work the other branch already had.
