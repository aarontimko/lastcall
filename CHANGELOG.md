# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the versions
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). A version's section here is
exactly what its GitHub release notes carry, so it is written for the people installing the
binary rather than for the commit log.

## Unreleased

### Changed

- **A plain `draft_dirs` entry now reviews the files in that folder only; add `/**` to review
  the folder and everything below it.** A watched folder that has grown a deep tree of
  scratch files no longer puts the whole of it on your screen. A record written by an earlier
  version is trimmed to the folder at the next scan, with the notice `N paths outside the
  root's scope dropped from its record`; widening the entry again brings those files back as
  new.
- **Watched folders no longer read files of `collapse_size_bytes` (512 KiB by default) and
  larger.** Nothing that big is opened, so a captured archive or a model file cannot turn
  into a screenful of diff. A file lastcall had already recorded still shows once when it
  changes, as a row reading `not read (over 512 KiB)` that you accept like any other; the
  rest are counted in a single line, `3 files over 512 KiB not read`.
- `*` in a `draft_dirs` entry no longer matches across `/`, so `notes/*` means the folders
  directly inside `notes` and nothing deeper. A `**` component still reaches further.
- A `draft_dirs` entry a 0.3.0 binary accepted is now refused at start, with a message naming
  the entry: `**` or `/**` on its own, which would have watched every folder under a parent
  directory, and a relative entry more than four folders below one, which is deeper than the
  search ever looks. Name the folder instead (`notes`, `notes/**`) or give the pattern a fixed
  part (`*_drafts`, `**/notes`).

### Added

- `draft_dir_parents` says how many parent folder names a watched folder carries in the list,
  one by default, so two folders of the same name are told apart: `repo/z_ignore` rather than
  `z_ignore`. `0` is the folder's path relative to where it was found, which is the bare name
  for an entry such as `notes` and `a/notes` for one such as `*/notes`; up to `4` for more.
- Selecting a repository's or a folder's own row shows where it is, on a dim second line
  above the pane, with your home directory written as `~`.

### Fixed

- **Inside a herdr pane, the workspace scope no longer hides a repository's watched folders.**
  A watched folder such as `repo/z_ignore` now stays listed with the repository it lives
  in; before, only `w` brought it back. A watched folder inside a repository the scope
  hides is still hidden with it.
- Files of exactly `collapse_size_bytes` collapse, as the docs said. Until now the limit
  itself was read as an ordinary diff.

## 0.3.0 - 2026-09-17

### Changed

- **What you accept is remembered per branch.** Each branch of a repository now keeps its own
  record of what you have seen. Accept work on a feature branch, check the branch you started
  from back out, and you get that branch's own pile, not a screen of files you already dealt
  with. The first time you check a branch out, the record carries across from the branch you
  came from, so the switch itself shows nothing new, and the work you had already accepted on
  the branch you left is folded away rather than listed as a screen of deletions, as long as
  what the new branch holds at those paths is something you have already seen or something
  that was in the repository before lastcall first opened it. A branch cut from your main line
  while you were working on another one shows its own commits and nothing else. A branch
  switch never marks content as seen that was not on your screen: a version committed and
  reverted while you were not looking, or brought in by merging a branch you never checked
  out, shows on the branch that carries it.
  Going back to a branch you have been on before brings its pile back exactly as you
  left it, flags and undo included. A change that reaches another branch by cherry-pick shows
  once more on that branch, because lastcall never assumes you have read it somewhere else.
  Deleting a branch drops what it remembered; renaming the branch you are on keeps it.
- `lastcall status --json` gains two fields per repository, `seen_branch` and
  `parked_branches`; every other field still describes the branch you are on. The report's
  `status_version` is unchanged.
- The state file for a repository is now schema 1.2. It is read and written in place by this
  version and needs nothing from you. An older lastcall can still open it and work on the
  branch it is on; the first thing it writes there drops the other branches' records, which
  costs a screen of already-seen files the next time you switch, never a missing one.

### Fixed

- Files accepted on a branch no longer come back as deletions on the branch you return to.
  Before this, one record covered the whole repository, so checking out a branch without those
  files showed every one of them as deleted and waiting to be accepted again.
- A notice printed while git was switching branches could pair one branch's name with the
  other's commit, reading `switched main → main`. The branch and its commit are now read as
  one state.

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
