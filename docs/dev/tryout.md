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
```

It was first built by hand for the word wrap phase's hands-on gate, and that one run found
a real defect no test had: Option-z on a Mac terminal with stock settings arrives as the
character `Ω`, not as `alt-z`, so the advertised key did nothing. That is the kind of fact
a packaged run exists to surface.

## What a run leaves on disk

A fresh directory under the system temp directory, `lastcall-tryout-<scenario>-<random>/`:

| Path | What it is |
|---|---|
| `parent/` | the repositories (and watched folders) the scenario built; the only entry in `parent_dirs` |
| `state/` | the state directory for this run: ledgers, stores, and a seeded `first-launch.json` so the welcome overlay does not open over step 1 |
| `state/config.toml` | `parent_dirs`, whatever the scenario added, and `[update] check = false` |
| `STEPS.md` | the numbered steps, as printed |
| `run.py` | reopens the same sandbox: `python3 <sandbox>/run.py` |

Nothing is deleted afterwards. The path is printed; the directory is yours to remove, and
it is also the evidence if a step failed: the state directory can be read with the `jq` and
`git` recipes in [`engine.md`](engine.md).

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
- `sandbox.welcome = True` leaves the first-launch welcome in place, for a scenario that is
  about the welcome.
- The function's docstring's first line is what `just tryout list` prints.

Rules a scenario follows:

1. **Deterministic and offline.** No random content, no network, nothing read from outside
   the sandbox, and nothing that depends on the date. Two runs of one scenario build the
   same files and the same history (commit times aside).
2. **Every step says what to do and what to see.** "Open `min.js`: the line ends in a dim
   ` … +N`" can pass or fail. "Check that wrapping works" cannot. What is seen must be
   there at any terminal size: the hint line drops entries in a narrow terminal (`wrap` is
   the first to go, under 150 columns with the diff focused), so a step never rests on a
   hint alone.
3. **Put a landmark at the far end of anything long.** The wrap scenario ends its long
   lines in `THE-END-OF-THE-PROSE-LINE` and `END-OF-MINIFIED`, so "is the end visible" is
   a word to look for, not a judgement.
4. **Check the keys against `DEFAULT_KEYMAP`** in `crates/lastcall/src/tui/input.rs` before
   writing them into a step, and say the laptop spelling where there is one (`End` is
   fn-Right). A step naming a key that is not bound wastes the run.
5. **Name the run that matters.** If the feature can differ inside a herdr pane, or on a
   second terminal, say so in a step. The footer already asks for both a standalone run and
   one inside herdr.
6. **Nothing personal and nothing real.** File names, content and repository names are
   invented. A scenario never copies from a real repository.
7. **One scenario per thing being judged.** A scenario that has grown past a dozen steps
   is two scenarios.

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
