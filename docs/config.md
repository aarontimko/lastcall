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

**Editing the file while the review screen is open.** Press `R` (the `reload` action) and
the file is read again, from the same place as at launch. A file that does not load, or a
`[keys]` table that does not parse, changes nothing: the status line says
`config not reloaded:` and the reason, and the running settings stay. A file that loads is
applied whole: the key bindings, `[ui]` values you changed in the file (a toggle you
flipped with a key keeps its state unless the file's value changed), the patterns, and the
list of repositories and watched folders, which is found again at once. Nothing you
accepted is touched, and the status line says what changed, for example
`config reloaded: 1 root added, keys`. A few settings are read once, at launch: `[herdr]`
and `[update]` (the status line says so when you changed them), the state directory, and
which file is the config file. `draft_initial` applies to watched folders listed after the
reload; one already listed keeps what it has.

## The first launch

The first time you open the review screen, a small card sits over the frame and names the
keys you need to get started. The frame underneath is live, so the scan you launched keeps
running while you read. `enter` moves to the next card, `q` skips the rest, and once you
have been through it the card never appears again: a one-line note in the state directory
records that it has been shown. `lastcall tui --tour` brings it back whenever you want it.

The card after the keys asks how far down the list looks: one folder below the directory
you launched in, which is what lastcall does today, or two, which also reaches a clone or a
worktree kept in a folder such as `worktrees/<name>`. Choosing two writes `search_depth = 2`
and the new repositories appear on the screen behind the card. It names the config file for
the settings that go further than that.

Two more cards appear only when they apply, and each offers a choice:

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
the session. Once you have added a top-level line yourself, `R` applies it; a `[herdr]` line
applies at the next launch.

## The keys

| key | default | what it does |
|---|---|---|
| `parent_dirs` | `[]`, meaning the directory you launched in | absolute paths. Every git repository directly under each one is watched, and a repository sitting untracked inside one of those is listed too, with a badge. A repository one plain folder deeper (for example `worktrees/<name>`) is reached with `search_depth`, or with its own entry here when it lives somewhere else entirely. A repository's review state is kept per entry here: one that comes to be found under a different entry (say it was reached through its parent folder and is now an entry of its own) starts afresh, as on the day it was first listed, and its old state is left on disk where it was. `R` and a relaunch agree on this. |
| `draft_dirs` | `[]` | folders that are **not** git repositories, each reviewed as a root of its own. Entries are absolute paths, or globs matched under every parent directory **and under every repository that was found**, so `"z_ignore"` names the `z_ignore` folder of each repository as well as one directly inside a parent directory, and `"*/scratch"` reaches one folder further down from each of those. A plain entry reviews **the files in that folder**, and nothing below it; add `/**` to review the folder and everything below it, minus any git repository inside it and anything an inner entry of its own already reviews. `*` matches within one folder name and never across a `/`; `**` as a whole component reaches up to four folders down, and an entry may name at most four folders. A file of `collapse_size_bytes` or more is never read in a watched folder, whatever the shape: it is counted in one notice instead. |
| `include_gitignored` | `[]` | Files your `.gitignore` hides are normally not reviewed. A pattern here makes matching gitignored files show as ordinary rows in their repository, with their diffs, so an agent writing into an ignored folder is still reviewed. Written exactly like a `.gitignore` line, and matched from the repository's own folder: `"z_ignore_*"` lists every gitignored file of that name at any depth, `"/notes-*.md"` only the ones at the top, and `"**/*.scratch.md"` works as it does in a `.gitignore`. A matching file is an ordinary row under its repository, new files pending as any untracked file is, with no badge. Git never looks inside an ignored folder, so a file inside one stays unlisted whatever the pattern says: to review a whole ignored folder, name the folder with a trailing slash (`"z_ignore/"`); `"z_ignore/**"` cannot reach inside it. A pattern that reaches an ignored repository inside yours, or the ignored folder holding one (`"z_ignore/"` for a clone in `z_ignore/dependencies/`), lists that repository as a nested repository with a row of its own, which `skip_globs` can take out again. An entry may not be empty, may not begin with `!` (every entry already re-includes), and is one line; write a trailing space as `\ `, as in `.gitignore`. Removing an entry later never hides anything: a file you accepted while it was in force stops being listed, but a later change to it, or its deletion, is still a row until you accept that too. |
| `draft_dir_parents` | `1` | how many folders above a watched folder its name shows, `0` to `4`. With `1`, a `z_ignore` folder inside a repository is listed as `repo/z_ignore`, which is what tells two folders of the same name apart. `0` is the matched folder's path relative to the directory it was found under, so an entry of `notes` shows `notes` and an entry of `*/notes` shows `a/notes`. |
| `draft_initial` | `"seen"` | what the first sight of a watched folder means. `seen` starts from zero, so only changes made after that are pending. `pending` treats everything already there as pending. |
| `collapsed_globs` | the nine common lockfiles | paths shown as one collapsed row instead of a wall of hunks. The default list is `package-lock.json`, `yarn.lock`, `pnpm-lock.yaml`, `Cargo.lock`, `poetry.lock`, `uv.lock`, `Gemfile.lock`, `go.sum`, `composer.lock`. Setting the key replaces the list. Each entry is matched from the repository's own folder, a bare name matches at any depth, and a `*` crosses folders here. |
| `collapse_size_bytes` | `524288` (512 KiB) | files at or above this size collapse too. Must be greater than zero. A file with a NUL byte in its first 8,000 is binary and collapses whatever this says. This is also the size at which a file in a watched folder stops being read at all, so raising it to see larger diffs in your repositories also makes watched folders read larger files, and their records grow with them. |
| `watch_ignore_globs` | `.git/**`, `node_modules/**`, `target/**`, `vendor/**`, `.venv/**` | While lastcall is open it watches your files so the list updates as agents write. Folders matching these globs do not wake the watcher, which stops build output and dependency folders from causing constant rescans. This is only about noise: a file that changed under a matching folder still shows at the next scan or when you press `r`. Each entry is matched against a changed file's path from the repository's own folder, a bare name matches at any depth, and a `*` crosses folders here (it does not in `skip_globs` or `draft_dirs`). The key was `ignore_globs` before this release. |
| `skip_globs` | `[]` | Names repositories and folders to leave out of the list. Without it, the list holds every repository found under where you launched, every clone left untracked inside one of those, every worktree kept inside one, and every repository inside a watched folder. A matching repository is never listed or opened, a matching plain folder is never searched, and inside a watched folder a matching file or clone is not reviewed. Matched against the path below the folder a repository is filed under; a `*` stays inside one folder name and `**` crosses folders. It never hides a changed file inside a repository you do see. Whichever way a repository was found, a match leaves it out before it is opened, so a folder full of clones costs nothing at launch: `"*/z_ignore/**/evals/**"` takes out every clone kept below a repository's `z_ignore/.../evals/` folder. A tracked or untracked file inside a listed repository is reported whatever the key says, because no key hides a real change. A parent directory itself and the directory you launched in are never skipped; a notice says the pattern was ignored for it. A skipped repository keeps whatever state lastcall already had for it, and comes back if you remove the entry. An entry is relative, may not be empty, and may not contain `..` or start with `~`. Worked examples, with directory trees, are in [Which key leaves what out](#which-key-leaves-what-out) below. |
| `hide_empty_repos` | `false` | what the `t` toggle starts as. `false` lists every repository under a parent directory, whether it has anything pending or not. `true` opens with the empty ones hidden, except one carrying an agent flag. `t` flips it for the session, and the headless commands are unaffected. The welcome on your first launch offers to write `true` here for you. |
| `search_depth` | `1` | how many folders below each parent directory are read for a repository. `1` is the repositories directly inside it, `2` also reads one plain folder further, such as `worktrees/<name>`, and lists a worktree kept inside a listed repository (one git call per listed repository, each rescan), up to `4`. The walk never enters a repository, so no depth reaches a clone kept inside one (a gitignored `z_ignore/dependencies/` say); a watched folder or `include_gitignored` is how such a clone is listed, and `skip_globs` is how it is left out again. Nor does the walk enter a dependency folder such as `node_modules`, `target`, `.venv` or `vendor`, and it runs again every thirty seconds, so `3` and `4` want a narrow parent directory rather than a home directory. The welcome on your first launch offers to write `2` here for you. A 0.1.0 binary refuses a config file that has it, so delete the line before going back to that version. |

```toml
parent_dirs = ["/home/me/src"]
draft_dirs = [
  "notes",            # the files in notes/, and nothing below it
  "_drafts/**",       # _drafts/ and everything below it
  "z_ignore",         # the scratch folder of each repository, its own files only
]
draft_dir_parents = 1  # listed as `repo/z_ignore`, not `z_ignore`
draft_initial = "seen"
collapsed_globs = ["package-lock.json", "Cargo.lock", "*.min.js"]
collapse_size_bytes = 524288
hide_empty_repos = false
search_depth = 1
```

If you have a config file and launch somewhere outside `parent_dirs`, that directory is
watched for the session anyway and a notice says so.

## Which key leaves what out

**Where a repository comes from.** A repository reaches the list in one of four ways:

1. The folder walk: every repository directly under each parent directory (the directory you
   launched in, when `parent_dirs` is empty) is listed, and with `search_depth` above `1`,
   every one under the plain folders further down, though the walk never enters a repository.
2. A clone inside a listed repository: when the scan of a repository finds another
   repository sitting untracked in it, that one gets a row of its own, badged as nested in
   the first.
3. A worktree kept inside a listed repository: with `search_depth` of `2` or more, each
   linked worktree a repository keeps inside its own folder gets a row, badged as a worktree
   of it.
4. A repository inside a watched folder: when a `draft_dirs` folder holds a clone, the clone
   gets a row of its own, badged as nested, like the second way.

`skip_globs` is checked on every candidate from all four ways before the repository is
opened, so a match costs no git call and never reaches the screen.

**Five keys take patterns, and each reads them its own way.**

| key | matched from | `*` | `**` | example |
|---|---|---|---|---|
| `draft_dirs` | each parent directory, and each repository found | stays inside one folder name | a whole component, up to four folders down | `"z_ignore/**"` |
| `include_gitignored` | the repository's own folder, as a `.gitignore` line is; a pattern with no `/` matches at any depth | as in `.gitignore`: stays inside one folder name | as in `.gitignore` | `"z_ignore_*"`, `"z_ignore/"` |
| `skip_globs` | the parent directory a repository is filed under (a parent directory, or the folder above a repository found anywhere else) | stays inside one folder name | crosses folders | `"archive/**"` |
| `watch_ignore_globs` | the repository's own folder; a bare name matches at any depth | crosses folders | crosses folders | `"target/**"` |
| `collapsed_globs` | the repository's own folder; a bare name matches at any depth | crosses folders | crosses folders | `"crates/app/tests/snapshots/**"` |

**A folder of old clones.**

```
/home/me/src/
├── app/            .git   (active)
├── api/            .git   (active)
├── archive/
│   ├── old-site/   .git
│   ├── prototype/  .git
│   └── ... 30 more
└── mirrors/
    └── upstream-x/ .git   (a read-only mirror)
```

Launched in `/home/me/src` with `search_depth = 2`, the walk finds `app` and `api` at the
first level and every clone under `archive/` and `mirrors/` at the second. All 35 are opened
at launch, several git calls each, and all 35 get a row; `t` hides the ones with nothing
pending, but they were still opened, and a stray file in one puts it back on screen. With

```toml
skip_globs = ["archive/**", "mirrors/**"]
```

the walk never enters those two folders: nothing there is opened or listed, and launch is
two repositories.

**A clone an agent left inside a repository.**

```
/home/me/src/
└── app/                    .git
    ├── src/
    └── tools/
        └── linter/         .git   (cloned here by an agent; not in .gitignore)
```

Launched in `/home/me/src`. `tools/linter/` is untracked and not ignored, so the scan of
`app` sees it and lastcall gives it a row of its own, badged as nested in `app`; a folder
holding a repository is never a file row of `app` itself, because git does not look inside
one. With

```toml
skip_globs = ["app/tools/linter/**"]
```

the clone shows nowhere: no row of its own, and no entry under `app`. Use the key for a
clone you do not want reviewed at all; what an agent writes inside it is not seen. The glob
starts with `app/` because paths are matched below `/home/me/src`, the folder `app` is filed
under, which is where this example launched.

**A clone inside a gitignored folder.** Had the agent cloned into `app/z_ignore/dependencies/linter/`
and `app`'s `.gitignore` named `z_ignore/`, neither door above would reach it: the walk does
not enter `app`, and git does not look inside an ignored folder, so the scan of `app` never
reports it. It reaches the list only through a watched folder (`draft_dirs = ["z_ignore/**"]`,
which watches each repository's `z_ignore/` and lists a clone inside it) or through an
`include_gitignored` pattern that reaches the ignored folder itself (`"z_ignore/"`; a
`"z_ignore/**"` does not). `skip_globs` carves it out of what those let in:
`skip_globs = ["app/z_ignore/dependencies/**"]` is the same key and the same rule, through a
different door.

`search_depth` counts plain folders under a parent directory, never what is inside a
repository: no depth reaches a clone kept inside one.

To see a set of generated files as short rows instead of their diffs, snapshots or
lockfiles say, use `collapsed_globs` with the folder (`"crates/app/tests/snapshots/**"`):
each file stays a row you can accept, with its counts, and no diff is read for it.

No key hides a change inside a repository you see: a tracked or untracked file that changed
is always a row, so that nothing an agent did goes by unreviewed. To accept many rows at
once, `shift-a` on a repository's row accepts everything pending in it, `shift-a` on a run
of rows selected with `shift-j` or `shift-k` accepts just those, and `ctrl-a` accepts
everything everywhere.

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
and shift with tab is `backtab`. That holds for ASCII letters only: any other character is
the key exactly as written, so `Ω` and `ω` are two different keys and each is written as
the character the keyboard sends.

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
| `extend_down` | `shift-j`, `shift-down` | from a file row, select the next file row as well, making a run of rows in one repository that `shift-a` accepts as one. Stops at the repository's last file row |
| `extend_up` | `shift-k`, `shift-up` | the same run, grown upward |
| `open` | `enter`, `l`, `right` | open the diff for the selected row |
| `back` | `esc`, `h`, `left` | close the help overlay, else return to the file list. Never quits. |
| `focus_toggle` | `tab` | move focus between the two panes |
| `hunk_next` | `n`, `]` | next hunk |
| `hunk_prev` | `p`, `[` | previous hunk |
| `expand` | `e` | expand a collapsed file into real hunks |
| `toggle_full_paths` | `f` | full paths instead of basenames |
| `toggle_remote` | `o` | show `org/repo` instead of the directory name |
| `wrap` | `Ω`, `alt-z` | wrap long lines in the diff, or clip them at the pane's edge |
| `hide_empty` | `t` | hide or show repositories with nothing pending |
| `snooze` | `s` | set a repository aside for a number of days |
| `show_snoozed` | `shift-s` | show or hide the repositories that are set aside |
| `accept` | `a` | accept the hunk under the cursor, or a file with no hunks |
| `accept_file` | `shift-a` | accept the whole file, the whole repository from its row, or every file of a selected run |
| `accept_all` | `ctrl-a` | accept everything listed, across every repository |
| `restore` | `u` | put the hunk back the way it was |
| `restore_file` | `shift-u` | put the whole file back, asking first |
| `undo` | `z` | undo the last accept in the selected repository; the last 20 are kept |
| `flag` | `m` | flag it with a note |
| `unflag` | `shift-m` | clear that file's flags |
| `select` | `v` | start a line selection in the right pane |
| `copy` | `y` | copy the selection, or the hunk under the cursor; on a repository or group row, that whole pane |
| `edit` | `i` | edit the file in place |
| `edit_external` | `shift-i` | open the file in your own editor at the hunk |
| `ack` | `d` | acknowledge an agent's attention flag |
| `jump` | `g` | jump to that agent in herdr |
| `scope` | `w` | turn the workspace scope on or off |
| `refresh` | `r` | rescan now |
| `reload` | `shift-r` | read the config file again and apply it (see the top of this page) |
| `help` | `?` | the help overlay, which lists all of this live |
| `quit` | `q`, `ctrl-c` | quit |

On macOS the Cmd key never reaches a program running in a terminal, so no action here can be
bound to it; `Home` and `End` on a Mac laptop keyboard are Fn-Left and Fn-Right, which iTerm2
and Terminal.app send as `home` and `end`. That is also why the two repository jumps are
bound to Option and an arrow: iTerm2 and herdr panes send that as
`alt-up` and `alt-down`. Terminal.app sends Option-arrow as a word jump instead, and its
"Use Option as Meta key" setting is not the answer: a terminal that sends Option as an
escape prefix can deliver Option-Up as three separate keys, and the third of them is `A`.
Use `{` and `}` there; they are bound to the same two actions and work everywhere.

`wrap` has no plain key by default: Option-z (`alt-z`) is the wrap key in VS Code
and the editors built on it, wrapping is on by default, and the letters still unbound are
kept for actions to come. A Mac terminal as it is set up out of the box does not send
Option as Alt; it sends the character the keyboard makes, and for Option-z on a US layout
that is `Ω`. So `Ω` is bound too, and Option-z works on a Mac with no settings change; the
help overlay shows it as `Opt-z (Ω)`. On another layout Option-z is a different character:
bind that one, `wrap = ["alt-z", "<it>"]`. A character outside ASCII is taken as typed, so
`Ω` and `ω` are different keys. A terminal that splits the escape delivers `Esc` then `z`,
and `z` is undo: there, give wrap a plain key of your own, `wrap = ["c"]` in `[keys]`.

The confirmation modal's own keys, `y` and `enter` to confirm, `n` and `esc` to cancel, are
not rebindable in this version.

**Three things are rejected**, by `lastcall config` as well as by the UI, so you find out
before you are in a full screen: an action name that does not exist, a spec the grammar
cannot parse, and a key bound to two actions once your entries are merged with the
defaults. Each error names the offending entry.

## `[ui]`

| key | default | what it does |
|---|---|---|
| `wrap` | `true` | what the wrap toggle (Option-z, `alt-z`) starts as. `true` wraps a diff line too long for the pane onto as many rows as it needs, breaking at a space where there is one. `false` opens clipping it at the pane's edge, the way versions before 0.5.0 did. Option-z (`alt-z` off a Mac) flips it for the session, and nothing is written back. |

```toml
[ui]
wrap = true
```

A line still cannot take the whole pane: past a few rows short of it the line stops and the
last row ends in a dim count of the characters not shown, so whatever follows the line is
always reachable. `y` copies lines, not rows, so a line that was wrapped or cut short on
screen arrives on the clipboard whole, with no break in it. The pane never scrolls
sideways, and the in-place editor (`i`) does not wrap.

`[ui]` is new in 0.5.0: a 0.4.0 binary refuses a config file that has the table, so delete
it before going back to that version.

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
