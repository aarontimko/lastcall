# Configuration

lastcall runs with no configuration at all: the directory you launch it in becomes the
directory it watches, and every key below has a default. A config file is how you point it
at somewhere else, rebind a key, or turn something off.

`lastcall config` prints the effective values, the file they came from (or
`(built-in defaults, no config file)`), the state directory, and any notices.
`lastcall config --json` prints the same thing for a script.

## Where the file lives

The first of these that exists wins:

1. `$LASTCALL_CONFIG`, if set. This one is explicit, so a path that does not exist is an
   error on every command rather than a silent fallback.
2. `$XDG_CONFIG_HOME/lastcall/config.toml`
3. `~/.config/lastcall/config.toml`
4. built-in defaults, if none of the above exists. Not an error.

Every key is optional. **An unknown key is an error**, on every command, naming the key and
the line: a typo in a config file never silently does nothing.

## The first launch

The first time you open the review screen, a small card sits over the frame and names the
keys you need to get started. The frame underneath is live, so the scan you launched keeps
running while you read. `enter` moves to the next card, `q` skips the rest, and once you
have been through it the card never appears again: a one-line note in the state directory
records that it has been shown. `lastcall tui --tour` brings it back whenever you want it.

Two of the cards appear only when they apply, and each offers a choice:

- Running inside a herdr session, lastcall narrows the list to the repositories that
  workspace is working in. The card offers to show every repository instead, which writes
  `scope = "all"` under `[herdr]`.
- With ten or more repositories that have nothing pending, the card offers to open with the
  empty ones hidden, which writes `hide_empty_repos = true`.

Either choice takes effect immediately and is written to the config file named in "Where the
file lives", under a comment saying where the line came from. If there is no config file
yet, one is created holding just that comment and that setting. **Nothing else in the file is
touched**: your comments, your key order and your formatting survive the edit. If the write
fails, the card says so and prints the line to add by hand, and the change still holds for
the session.

## The keys

| key | default | what it does |
|---|---|---|
| `parent_dirs` | `[]`, meaning the directory you launched in | absolute paths. Every git repository under each one is watched. |
| `draft_dirs` | `[]` | directories that are **not** git repositories, each reviewed as a root of its own. Globs relative to a parent directory (`"_drafts/**"`, `"notes"`) or absolute paths. |
| `draft_initial` | `"seen"` | what the first sight of a draft root means. `seen` starts from zero, so only changes made after that are pending. `pending` treats everything already there as pending. |
| `collapsed_globs` | the nine common lockfiles | paths shown as one collapsed row instead of a wall of hunks. The default list is `package-lock.json`, `yarn.lock`, `pnpm-lock.yaml`, `Cargo.lock`, `poetry.lock`, `uv.lock`, `Gemfile.lock`, `go.sum`, `composer.lock`. Setting the key replaces the list. |
| `collapse_size_bytes` | `524288` (512 KiB) | files at or above this size collapse too. Must be greater than zero. A file with a NUL byte in its first 8,000 is binary and collapses whatever this says. |
| `ignore_globs` | `.git/**`, `node_modules/**`, `target/**`, `vendor/**`, `.venv/**` | these scope the filesystem watcher only. An ignored path never wakes a scan, but the next scan still reports a tracked edit under it, so this is a noise filter and not a way to hide changes. |
| `hide_empty_repos` | `false` | what the `t` toggle starts as. `false` lists every repository under a parent directory, whether it has anything pending or not. `true` opens with the empty ones hidden, except one carrying an agent flag. `t` flips it for the session, and the headless commands are unaffected. The welcome on your first launch offers to write `true` here for you. |

```toml
parent_dirs = ["/home/me/src"]
draft_dirs = ["_drafts/**", "notes"]
draft_initial = "seen"
collapsed_globs = ["package-lock.json", "Cargo.lock", "*.min.js"]
collapse_size_bytes = 524288
hide_empty_repos = false
```

If you have a config file and launch somewhere outside `parent_dirs`, that directory is
watched for the session anyway and a notice says so.

## `[keys]`

One entry per action: a key spec, or a list of them. An entry **replaces** that action's
default bindings rather than adding to them.

```toml
[keys]
quit = "ctrl-q"
hunk_next = ["n", "ctrl-n"]
```

**The grammar.** Optional `ctrl-`, `alt-` and `shift-` prefixes, then either a single
character or a named key: `up down left right pageup pagedown home end enter esc tab
backtab space backspace delete f1` through `f12`. Specs are case-insensitive, so `Ctrl-C`
and `ctrl-c` are the same thing and `K` is `k`. An upper-case letter is written `shift-k`,
and shift with tab is `backtab`.

**The actions**, with what they do and what they are bound to out of the box:

| action | default | what it does |
|---|---|---|
| `nav_up` | `up`, `k` | previous entry, or scroll the diff up |
| `nav_down` | `down`, `j` | next entry, or scroll the diff down |
| `nav_page_up` | `pageup`, `b` | a page up |
| `nav_page_down` | `pagedown`, `space` | a page down |
| `nav_top` | `home` | the first entry, or the top of the diff |
| `nav_bottom` | `end` | the last entry, or the end of the diff |
| `nav_prev_root` | `alt-up`, `{` | the previous repository's row. Inside the first one, that repository's own row |
| `nav_next_root` | `alt-down`, `}` | the next repository's row. On the last one, nothing: these jump, they never wrap |
| `open` | `enter`, `l`, `right` | open the diff for the selected row |
| `back` | `esc`, `h`, `left` | close the help overlay, else return to the file list. Never quits. |
| `focus_toggle` | `tab` | move focus between the two panes |
| `hunk_next` | `n`, `]` | next hunk |
| `hunk_prev` | `p`, `[` | previous hunk |
| `expand` | `e` | expand a collapsed file into real hunks |
| `toggle_full_paths` | `f` | full paths instead of basenames |
| `toggle_remote` | `o` | show `org/repo` instead of the directory name |
| `hide_empty` | `t` | hide or show repositories with nothing pending |
| `snooze` | `s` | set a repository aside for a number of days |
| `show_snoozed` | `shift-s` | show or hide the repositories that are set aside |
| `accept` | `a` | accept the hunk under the cursor, or the selected entry |
| `accept_file` | `shift-a` | accept the whole file |
| `accept_all` | `ctrl-a` | accept everything listed, across every repository |
| `restore` | `u` | put the hunk back the way it was |
| `restore_file` | `shift-u` | put the whole file back, asking first |
| `undo` | `z` | undo the last accept in the selected repository; the last 20 are kept |
| `flag` | `m` | flag it with a note |
| `unflag` | `shift-m` | clear that file's flags |
| `select` | `v` | start a line selection in the diff |
| `copy` | `y` | copy the selection, or the hunk under the cursor |
| `edit` | `i` | edit the file in place |
| `edit_external` | `shift-i` | open the file in your own editor at the hunk |
| `ack` | `d` | acknowledge an agent's attention flag |
| `jump` | `g` | jump to that agent in herdr |
| `scope` | `w` | turn the workspace scope on or off |
| `refresh` | `r` | rescan now |
| `help` | `?` | the help overlay, which lists all of this live |
| `quit` | `q`, `ctrl-c` | quit |

On macOS the Cmd key never reaches a program running in a terminal, which is why the two
repository jumps are bound to Option and an arrow: iTerm2 and herdr panes send that as
`alt-up` and `alt-down`, while Terminal.app sends it as a word jump unless its profile has
"Use Option as Meta key" turned on, so `{` and `}` are bound to the same two actions and
work everywhere.

The confirmation modal's own keys, `y` and `enter` to confirm, `n` and `esc` to cancel, are
not rebindable in this version.

**Three things are rejected**, by `lastcall config` as well as by the UI, so you find out
before you are in a full screen: an action name that does not exist, a spec the grammar
cannot parse, and a key bound to two actions once your entries are merged with the
defaults. Each error names the offending entry.

## `[herdr]`

The overlay that appears when lastcall is running inside a [herdr](https://github.com/herdrdev/herdr)
session. What it adds and how the link is found: [`herdr.md`](herdr.md).

| key | default | what it does |
|---|---|---|
| `mode` | `"auto"` | `auto` links to herdr when there is a session to link to and runs standalone otherwise, `on` also says in the header why a link failed, `off` never looks. |
| `session` | unset | pin a named session instead of discovering one. |
| `toast` | `true` | ask herdr for a desktop notification when a repository first goes ready. herdr's own `[ui.toast] delivery` must be set to `"herdr"` as well, and it is `"off"` by default. |
| `scope` | `"workspace"` | which repositories the overlay covers: `workspace` narrows to the ones in the herdr workspace this pane belongs to, `all` covers every watched repository. `w` toggles it for the session. The welcome on your first launch offers to write `all` here for you. |

```toml
[herdr]
mode = "auto"
session = "work"
toast = true
scope = "workspace"
```

## `[update]`

| key | default | what it does |
|---|---|---|
| `check` | `true` | once a day, in the background and only after the first screen has been drawn, the UI asks the GitHub releases API whether a newer version exists and shows `↑ <version>` in the header if one does. |

```toml
[update]
check = false
```

`check = false` is the only switch. There is deliberately no environment variable for it: a
setting that decides whether the program talks to the network belongs in a file you can
read, not in whatever a parent process happened to export. It governs the background check
alone. `lastcall update` and `lastcall update --check` are things you asked for by typing
them, and they run regardless.

## Environment variables

| variable | what it does |
|---|---|
| `LASTCALL_CONFIG` | use this config file. A path that does not exist is an error. |
| `LASTCALL_STATE_DIR` | where the review ledger lives. A relative value is resolved against the directory you launched in, and never through symlinks. |
| `LASTCALL_KEYBOARD` | `plain`, and only that exact spelling, skips the terminal capability probe at launch. Worth setting if your terminal answers nothing and you notice a pause before the first screen: see [`dev/bench.md`](dev/bench.md). Anything else, including an empty value, means "ask the terminal". |
| `LASTCALL_LOG_FILE` | append a log to this file. Unset means no log at all: lastcall never writes diagnostics to the terminal it is drawing in. |
| `LASTCALL_LOG` | the filter for that log, default `info`. `debug` is the useful one when something is not appearing. |
| `LASTCALL_PARALLELISM` | how many repositories are scanned at once. There is no config key for this. It exists so a test can run the same command at width 1 and width 8 and compare the output, and a value that is not a positive whole number is ignored rather than refused. |

`XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `HOME` take part in the two searches above.
`VISUAL` and `EDITOR` decide what `shift-i` opens, in that order, falling back to `vi`.
`CARGO_HOME` and `HOME` are read by `lastcall update` for one purpose: recognising a binary
that a package manager owns, which it refuses to replace. `HERDR_*` variables are set by
herdr itself when lastcall runs in one of its panes: [`herdr.md`](herdr.md).

### Two variables that exist for the tests

These are not features, and neither is useful outside a test or the install smoke check.
They are documented because an undocumented environment variable that touches the network
path is worse than a documented one.

- **`LASTCALL_UPDATE_BASE_URL`** points `lastcall update` at a different server. It is
  honoured only when it is loopback, exactly `http://127.0.0.1:<port>/` or
  `http://localhost:<port>/` with no path after the slash, and it announces itself on
  standard error every time it is used. Anything else is ignored with a warning that says
  so. The once-a-day background check never reads it at all. The reason for all three rules
  is the same: a variable that can aim a self-updater at any host is a way to install
  someone else's binary whose checksum verifies perfectly, because the checksum came from
  the same host, and something else in your environment may be setting variables for you.
  Loopback cannot be another machine. What it buys is the install check in
  [`dev/publishing.md`](dev/publishing.md), which serves a real release layout from a local
  web server inside a fresh container and then runs a real `lastcall update` against it, so
  the download, the checksum and the atomic replacement are exercised end to end without
  the network.
- **`LASTCALL_TEST_RELEASE_DIR`** is read by the stand-in `curl` that the test suite puts
  on the child's `PATH`. It names a directory laid out like the releases API, and every
  byte the test suite "downloads" comes from it. Leaving it unset is not a fallback to the
  real network: the stand-in exits with an error saying a scene tried to reach the network.
  That is what proves the test suite is offline. The suite never has a way to spend your
  rate limit or to depend on a release existing.

## The state directory

Everything lastcall remembers lives in one place outside your repositories:
`$LASTCALL_STATE_DIR`, else `$XDG_STATE_HOME/lastcall`, else `~/.local/state/lastcall`.
Under it, `roots/<parent-id>/repos/<root-id>/` holds one directory per watched repository,
each with a `ledger.json` recording what you have already seen, a `store/` that is a bare
git repository holding the baseline objects, a private `index` used as a cache, and a
`lock` file that serialises writes so two lastcalls over one repository keep each other's
accepts. Flags that had nowhere to go are written under `exports/`, and the once-a-day
update check leaves a timestamp in `update-check.json`, beside `first-launch.json`, the
one-line note that the welcome has already been shown. The identifiers are hashes of the
paths, so `ls` is the quickest way to find the one you want, and `lastcall status` prints
the state directory it read as its first line. Nothing is ever written inside a watched
repository. The full layout, and how to read a ledger with `jq` and `git`, are in
[`dev/engine.md`](dev/engine.md).

**The state directory only grows.** Each baseline you accept adds objects to that
repository's store, and nothing collects the old ones, so a directory you have been
reviewing for months holds every version it ever recorded. It is safe to delete: the whole
directory, or one repository's subdirectory. What you lose is the memory of what you have
already seen, which means the next scan treats everything currently uncommitted as pending
again. Nothing in your repositories is affected.
