# Packaged hands-on runs: `just tryout`

Some things can only be judged by a person at a keyboard: whether wrapped text reads well,
whether a key arrives the way the terminal on their desk sends it, whether a flow feels
right inside a herdr pane. Every phase has at least one gate item of that kind. The cost of
such a check used to be the setup: make a repository, commit a "before", edit files into an
"after", write a config, remember the environment variables, then remember what to look
for. People skip checks that cost ten minutes to start.

`just tryout <scenario>` removes that cost. One command builds the release binary, builds a
sandbox, prints numbered steps that say what to do and what each should show, and opens the
TUI over the sandbox.

```sh
just tryout list            # the scenarios, one line each
just tryout wrap            # build, print the steps, Enter opens the TUI
just tryout wrap --no-launch  # build and print only (an agent's shell, a quick check)
just tryout wrap --in=demo  # open from inside the `demo` repository, not from its parent
```

A run opens from the sandbox's `parent/`, which its config names in `parent_dirs`, so
every repository underneath is listed. The directory lastcall is opened from decides what
is watched only when the config names no `parent_dirs`, which is how it runs with no
config at all. `--in=demo` sets that up: it leaves `parent_dirs` out and opens from inside
`parent/demo`, so that working tree is the whole list. It takes any directory under
`parent/` and refuses one that is not there, naming the sandbox it had built.

It was first built by hand for the word wrap phase's hands-on gate, and that one run found
a real defect no test had: Option-z on a Mac terminal with stock settings arrives as the
character `Ω`, not as `alt-z`, so the advertised key did nothing. That is the kind of fact
a packaged run exists to surface.

## What a run leaves on disk

One directory per scenario under the system temp directory, `lastcall-tryout-<scenario>/`:

| Path | What it is |
|---|---|
| `parent/` | the repositories (and watched folders) the scenario built; the only entry in `parent_dirs` |
| `state/` | the state directory for this run: ledgers, stores, and a seeded `first-launch.json` so the welcome overlay does not open over step 1 |
| `state/config.toml` | `parent_dirs`, whatever the scenario added, and `[update] check = false` |
| `STEPS.md` | the numbered steps, as printed |
| `run.py` | reopens the same sandbox: `python3 <sandbox>/run.py` |
| `tui.pid` | written by `run.py`: the pid of the lastcall it opened, read by the next run |

Nothing is deleted afterwards. The path is printed; the directory is yours to remove, and
it is also the evidence if a step failed: the state directory can be read with the `jq` and
`git` recipes in [`engine.md`](engine.md).

The path is the same on every run, so a second terminal can `cd` to it once and stay
right across runs. The printout's `shell:` line is that `cd`, spelled out in full: a `cd`
built on `$TMPDIR` fails silently in a shell that does not carry the variable, and the
next command then runs wherever that shell was. A run moves the previous run's sandbox aside first, to
`lastcall-tryout-<scenario>.<built-at>/`, and says so under the paths (`note: the previous
cherry-pick sandbox was moved to …`); the old run's state is kept as evidence. A run refuses
to start while a lastcall opened by an earlier run is still open over the path (the pid in
`tui.pid` is alive and is a lastcall): that lastcall would otherwise carry on writing the
old run's state into the new run's directory. Quit it with `q` and run again.

The stable path replaced a random suffix. The Phase 14 hands-on walk was run three times
with a suffix per run, and each time the shell's git commands landed in an earlier run's
sandbox while the open lastcall watched the new one, so nothing changed on screen. A walk
that goes wrong is restarted from `just tryout <scenario>`, never patched midway: one
command, one directory, the steps from the top.

## What it never touches

- `~/.local/state/lastcall` and `~/.config/lastcall`. The run is pointed at the sandbox
  with `LASTCALL_STATE_DIR` and `LASTCALL_CONFIG`, the same two variables the probes use.
- The network. The scenario's config turns the daily update check off.
- Your git identity or config. The script's own git commands carry a throwaway identity
  on the command line, with signing and hooks off, and they do not read the global or
  system git config at all, so an excludes file or a line-ending setting of yours cannot
  change what a scenario builds. (lastcall itself, once open, runs git as it always does.)

`HOME` is deliberately **not** redirected, unlike `just probe-tui`. A hands-on run is often
about herdr: inside a pane lastcall is handed the session's socket, but from any other
terminal it looks for the session under the real home directory, and with `HOME` pointed
away it would come up standalone beside a running herdr.

## Writing a scenario

A scenario is one function in [`scripts/tryout.py`](../../scripts/tryout.py), registered in
`SCENARIOS`. It receives a `Sandbox`, builds what it needs, and returns the steps.

```python
def scenario_rename(sandbox):
    """A renamed file: the row, the similarity, accept and undo."""
    repo = sandbox.repo("demo")
    repo.commit("base", repo.write("old_name.rs", "fn main() {}\n" * 40))
    repo.git("mv", "old_name.rs", "new_name.rs")
    sandbox.config_extra(tables="[ui]\nwrap = false")
    return [
        "The row reads `new_name.rs  R` with `(renamed from old_name.rs 100%)`.",
        "Press `a`: the row leaves the list. Press `z`: it comes back.",
    ]
```

The pieces:

- `sandbox.repo(name)` makes `parent/<name>` and runs `git init` on branch `main`. Call it
  more than once for a multi-repository scene.
- `repo.write(path, text)` writes a file, making folders as needed, and returns the path so
  it can be handed to `commit`.
- `repo.commit(message, *paths)` commits exactly those paths. What is committed is the
  "before"; what is written after the last commit is what lastcall lists.
- `repo.git(...)` is any other git command in that repository (a branch, a rename, a stash,
  a conflict), with the throwaway identity applied.
- `sandbox.config_extra(keys=..., tables=...)` adds top-level keys (`draft_dirs = [...]`)
  or whole tables (`[keys]`, `[ui]`) to the config. A watched scratch folder is a
  `draft_dirs` entry plus files written under it with `repo.write`. `parent_dirs` and
  `[update]` are the sandbox's own, and a table can be given once (TOML's rule): the call
  raises on either, so the mistake shows when the scenario is built and not at launch.
- `sandbox.launch_in = "demo"` opens lastcall from inside that directory (relative to
  `parent/`), and `sandbox.name_parent = False` leaves `parent_dirs` out of the config,
  so lastcall watches the directory it is opened from, as it does with no config. Set
  both when the thing being judged depends on where it is opened (with `parent_dirs`
  named, the launch directory changes nothing). The person's `--in=` sets both.
- `sandbox.welcome = True` leaves the first-launch welcome in place, for a scenario that is
  about the welcome.
- The function's docstring's first line is what `just tryout list` prints.

Rules a scenario follows:

1. **Deterministic and offline.** No random content, no network, nothing read from outside
   the sandbox, and nothing that depends on the date. Two runs of one scenario build the
   same files and the same history (commit times aside).
2. **Every step says what to do and what to see.** "Open `min.js`: the line ends in a dim
   ` … +N`" can pass or fail. "Check that wrapping works" cannot. What is seen must be
   there at any terminal size (the help overlay folds under 97 columns, too): the hint line drops entries in a narrow terminal (`wrap` is
   the first to go, under 154 columns with the diff focused), so a step never rests on a
   hint alone.
3. **Spell it out.** A key is named with what it does and to what: "press `ctrl-a` (hold
   Control, press `a`: accept all), which accepts both `d.rs` and `e.rs`", never "press
   `ctrl-a`: empty". "Empty" says what is empty ("the `demo` list has no rows"), "no group"
   says what is missing ("no `seen` row: on `run-2` this content has never been accepted"),
   and a git command says what it does to the repository when the reason for the next
   screen depends on it. The sponsor's Phase 14 walk stalled at step 10 on exactly those
   two abbreviations.
4. **Put a landmark at the far end of anything long.** The wrap scenario ends its long
   lines in `THE-END-OF-THE-PROSE-LINE` and `END-OF-MINIFIED`, so "is the end visible" is
   a word to look for, not a judgement.
5. **Check the keys against `DEFAULT_KEYMAP`** in `crates/lastcall/src/tui/input.rs` before
   writing them into a step, and say the laptop spelling where there is one (`End` is
   fn-Right). A step naming a key that is not bound wastes the run.
6. **Name the run that matters.** If the feature can differ inside a herdr pane, or on a
   second terminal, say so in a step. The footer already asks for both a standalone run and
   one inside herdr.
7. **Nothing personal and nothing real.** File names, content and repository names are
   invented. A scenario never copies from a real repository.
8. **One scenario per thing being judged.** A scenario that has grown past a dozen steps
   is two scenarios.

## The scenarios

`just tryout list` is the authority; the steps are printed by the scenario itself and saved
as `STEPS.md` in the sandbox. What each one is for:

| Scenario | What it lets a person judge |
|---|---|
| `wrap` | word wrap in the diff pane: prose, code, other scripts, the cap, paging, `alt-z` |
| `ignored` | `include_gitignored`: gitignored scratch files listed under their repository, never from inside an ignored folder |
| `reload` | `R`: a watched folder uncommented and listed without a restart, an accept that survives it, `skip_globs` taking a repository out, a broken file refused |
| `cherry-pick` | the `[seen] N files` fold: a cherry-pick, a rebase and a squash-merge of reviewed work, the group opened with `e`, a member edited or flagged out of it, the group accepted and undone, and nothing hidden from `status` |

### `ignored`

One repository, `demo`, whose committed `.gitignore` ignores `z_ignore_*` and `z_ignore/`,
with `z_ignore_plan.md` at the top, `src/deep/er/z_ignore_notes.md` three folders down and
`z_ignore/inside.md` inside the ignored folder; the config says
`include_gitignored = ["z_ignore_*"]`. The steps: the two matching files are rows with no
badge and the one inside `z_ignore/` is absent; accept the top file; append a line to it
from a second terminal (the printed step names the sandbox path and the command) and see
one hunk; accept; delete it and accept the deletion; the deep file accepts with `A` and
comes back with `z`.

### `reload`

Two repositories, `demo` (one change to `app.py`) and `evals-clone` (one change), and two
plain folders, `notes` and `scratch`; the config lists the watched folders one per line,
with `# "scratch",` commented out. The steps: three roots; accept `app.py`; uncomment the
line (nothing moves until asked); `R` lists `scratch` with the notice
`config reloaded: 1 root added`, and `demo` still has nothing pending; a
`skip_globs = ["evals-clone"]` line at the top and `R` takes the clone out
(`1 root removed, skip_globs`) and deleting it brings it back; a last line that is not TOML
and `R` is refused on the status line with the line number and the reason (the file's
path is left off, since it would push the reason past an 80-column screen), every key
still working; mended, `R` says `nothing changed`; `?` and the hint line name `R`.

### `cherry-pick`

One repository, `demo`: `a.rs`, `b.rs` and `c.rs` committed on `main`; `run-2` commits
`d.rs` and `e.rs`; `feat-x`, `feat-y` and `feat-z` are cut from `main` before that; the
repository is left on `run-1` (from `main`) with the three files edited and uncommitted.
The repository's own git config carries an identity with signing and hooks off, because
the person runs git in it by hand from a second terminal while lastcall stays open. Every
git command is printed verbatim in the steps, with the sandbox path. The twelve steps, as
the scenario prints them (the engine half of each is pinned by
`seen_group_the_tryout_walk_gives_what_each_step_promises` in
`crates/lastcall-engine/tests/test_integration_seen_group.rs`):

1. The reviewed work: `demo` on `run-1 · 3 files`; read them, `ctrl-a`, the list empties;
   `git commit -qam "run-1 work"`, it stays empty.
2. The switch: `git switch feat-x`, the branch line says `feat-x`, the list stays empty.
3. The cherry-pick: `git cherry-pick run-1`, within a second or two one row,
   `[seen] 3 files`, not three.
4. Read it: select it, the right pane says `3 files, content accepted on run-1` and lists
   the three paths with `run-1` beside each.
5. Open it: `e`, the three files indented under the group row, badged `[seen]` (the hint
   line reads `e collapse` once the `HEAD moved` status line has cleared); select
   `b.rs`, its diff like any row's with `[seen]` on the header; `e` again folds them back
   and selects the group row.
6. One file changes: `printf 'extra\n' >> b.rs`, `b.rs` becomes its own row (no badge)
   above `[seen] 2 files`.
7. Flag a member: `e`, select `c.rs`, `m`, a note, Enter; `c.rs` leaves as its own row
   with the flag mark and `[seen]`; `[seen] 1 file` remains, still open.
8. Accept the group: select it, `a` is refused (`A accepts the group`), `A` says
   `accepted [seen] 1 file`, `z` says `undid accept of a.rs` and the group is back.
9. Accept all: ten scratch files
   (`for i in 1 2 3 4 5 6 7 8 9 10; do echo "note $i" > note-$i.txt; done`) so `ctrl-a`
   asks; the modal says `Accept all 13 files in demo?` and
   `0 grouped upstream · 1 grouped seen · 0 collapsed`; `y` empties the list; then
   `git add -A && git commit -qm "feat-x work"` so the next switch starts clean.
10. The rebase variant: `git switch run-2` shows `d.rs` and `e.rs`, `ctrl-a`;
    `git switch feat-y`, empty; `git rebase run-2`, `[seen] 2 files`, accepted on `run-2`.
11. The squash-merge variant: `git switch feat-z`, empty; `git merge --squash run-2`,
    `[seen] 2 files`.
12. Nothing hidden: `lastcall status` (the printed line carries the sandbox's
    `LASTCALL_STATE_DIR` and `LASTCALL_CONFIG`) lists `d.rs` and `e.rs` with `[seen]` and
    the line `[seen] 2 files`; `--json` shows a `seen_on` list holding `run-2` on each row
    and a group of kind `seen`.

The scratch files in step 9 are there because the accept-all modal only asks above ten
files; without them `ctrl-a` accepts at once and there is no count to read.

## Where it sits among the other tools

| Tool | Who runs it | What it answers |
|---|---|---|
| unit, integration, snapshot and PTY tiers ([`testing.md`](testing.md)) | CI and the hooks | is the behaviour what the tests say, on every commit |
| `just probe-*` | an agent or a developer | what does the built binary print or draw over the standard fixture |
| `just tryout <scenario>` | a person | does this feature, in this terminal, on this keyboard, read and feel right |

A defect a hands-on run finds goes back into the automated tiers: the Option-z finding is
now a PTY scene that sends `Ω` the way the terminal does. The scenario stays, so the next
change to that feature gets the same five-minute check.

## For a phase's hands-on gate

When a phase kickoff has a hands-on gate item, the phase's work includes its scenario: the
worker or the orchestrator adds `scenario_<feature>` alongside the feature, checks it with
`--no-launch` and `lastcall status` over the printed sandbox, and the hand-off to the
person is one line, `just tryout <feature>`. Their words on the run go in the spec's
decision log with the phase's close-out.
