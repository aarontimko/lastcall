#!/usr/bin/env python3
"""A packaged hands-on run: one command builds a sandbox and opens lastcall over it.

A feature that has to be judged by a person at a keyboard (a phase's hands-on gate item, a
bug someone reported, a pull request's "try it") should not cost that person ten minutes of
making repositories, editing files and writing a config first. A **scenario** here is that
setup as code, plus the numbered steps to follow and what each should show.

    tryout.py list                 the scenarios, one line each
    tryout.py <scenario>           build the sandbox, print the steps, open the TUI over it
    tryout.py <scenario> --no-launch
                                   build the sandbox and print the steps and the launch
                                   line, open nothing (what an agent or a test runs)
    tryout.py <scenario> --in=demo open lastcall from inside `parent/demo` (any path under
                                   `parent/`) with no `parent_dirs` in the config, so that
                                   directory is what it watches

`just tryout <scenario>` builds the release binary first and then runs this.

The sandbox is a fresh directory under the system temp directory holding the repositories
(`parent/`), a state directory (`state/`), a `config.toml` naming only `parent/`, the steps
(`STEPS.md`) and a `run.py` that reopens the same sandbox. lastcall is pointed at it with
LASTCALL_STATE_DIR and LASTCALL_CONFIG, so the real `~/.local/state/lastcall` and
`~/.config/lastcall` are never read or written. HOME is left alone on purpose: outside a
herdr pane lastcall looks for the herdr session under the real home directory. Nothing is
deleted afterwards; the path is printed, and the directory is the person's to remove.

Adding a scenario: write a function that takes a `Sandbox`, builds what it needs with
`sandbox.repo(...)`, `Repo.write`, `Repo.commit` and `sandbox.config_extra`, and returns the
steps; register it in SCENARIOS. docs/dev/tryout.md has the rules a scenario follows.

Standard library only, and nothing newer than Python 3.9 (what macOS ships).
"""

import json
import os
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BINARY = os.path.join(ROOT, "target", "release", "lastcall")


class Repo:
    """One git repository under the sandbox's parent directory."""

    def __init__(self, path):
        self.path = path
        os.makedirs(path)
        self.git("init", "-q", "-b", "main", ".")

    def git(self, *args):
        # The identity is the commit's own and the person's git config is not read at all
        # (a global excludes file or `core.autocrlf` would change what a scenario builds);
        # the hooks path is emptied so no hook runs here.
        env = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_SYSTEM="/dev/null")
        subprocess.run(
            [
                "git",
                "-c", "user.name=tryout",
                "-c", "user.email=tryout@example.invalid",
                "-c", "core.hooksPath=/dev/null",
                "-c", "commit.gpgsign=false",
                *args,
            ],
            cwd=self.path,
            env=env,
            check=True,
            stdout=subprocess.DEVNULL,
        )

    def write(self, name, text):
        """Write `text` to `name` (folders made as needed). Returns `name`."""
        full = os.path.join(self.path, name)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "w", encoding="utf-8") as f:
            f.write(text)
        return name

    def commit(self, message, *names):
        """Commit exactly the named files: what is committed is the 'before'."""
        self.git("add", "--", *names)
        self.git("commit", "-q", "-m", message)


class Sandbox:
    """The directory a scenario builds into, and the config it opens with."""

    def __init__(self, scenario):
        self.base = tempfile.mkdtemp(prefix="lastcall-tryout-%s-" % scenario)
        self.parent = os.path.join(self.base, "parent")
        self.state = os.path.join(self.base, "state")
        self.config = os.path.join(self.state, "config.toml")
        os.makedirs(self.parent)
        os.makedirs(self.state)
        # Extra TOML for the scenario: top-level keys first, then tables.
        self.config_keys = []
        self.config_tables = []
        # A fresh state directory opens on the first-launch welcome, which would sit over
        # step 1 and whose choices write to the config. A scenario about the welcome itself
        # sets this to True.
        self.welcome = False
        # Where lastcall is opened from, relative to `parent/`: "" is the parent directory
        # (every repository under it), "demo" is inside one repository's working tree. The
        # person's `--in=` overrides it.
        self.launch_in = ""
        # False leaves `parent_dirs` out of the config, so lastcall watches whatever
        # directory it is opened from: the way it runs with no config at all.
        self.name_parent = True

    def launch_dir(self):
        """The directory lastcall opens from; refused if it is not under `parent/`."""
        full = os.path.realpath(os.path.join(self.parent, self.launch_in))
        root = os.path.realpath(self.parent)
        if not (full == root or full.startswith(root + os.sep)) or not os.path.isdir(full):
            raise ValueError("no directory %r under the sandbox's parent/" % self.launch_in)
        return full

    def repo(self, name):
        return Repo(os.path.join(self.parent, name))

    def config_extra(self, keys="", tables=""):
        """Top-level `key = value` lines and whole `[table]` blocks for config.toml.

        `parent_dirs` and the `[update]` table are the sandbox's own; TOML allows a key or
        a table once, so a scenario that names either is refused here, not at launch.
        """
        named = [line.strip() for line in (keys + "\n" + tables).splitlines()]
        if any(n.startswith("parent_dirs") or n.startswith("[update]") for n in named):
            raise ValueError("parent_dirs and [update] belong to the sandbox")
        heads = [n for n in named if n.startswith("[")]
        taken = [n for t in self.config_tables for n in t.splitlines() if n.startswith("[")]
        if any(h in taken for h in heads) or len(set(heads)) != len(heads):
            raise ValueError("a table can be given once: put all of it in one call")
        if keys:
            self.config_keys.append(keys.strip("\n"))
        if tables:
            self.config_tables.append(tables.strip("\n"))

    def write_config(self):
        # The daily update check is off: a hands-on run makes no network request.
        # json.dumps writes a basic string TOML reads the same way (quotes, backslashes).
        lines = ["parent_dirs = [%s]" % json.dumps(self.parent)] if self.name_parent else []
        lines += self.config_keys
        lines += ["", "[update]", "check = false"]
        for table in self.config_tables:
            lines += ["", table]
        with open(self.config, "w", encoding="utf-8") as f:
            f.write("\n".join(lines) + "\n")
        if not self.welcome:
            # What lastcall writes when the welcome is dismissed (`tui/tour.rs`).
            with open(os.path.join(self.state, "first-launch.json"), "w", encoding="utf-8") as f:
                json.dump({"shown_at": 0, "version": "tryout"}, f)


# ---- scenarios -------------------------------------------------------------------------


def scenario_wrap(sandbox):
    """Word wrap in the right pane: prose, code, other scripts, the cap, paging, the keys."""
    repo = sandbox.repo("demo")
    prose = (
        "The quick brown fox jumps over the lazy dog and keeps going well past the edge of "
        "the pane so that the end of this sentence can only be read if the line wraps. "
    )
    names = [
        repo.write("notes.md", "# Notes\n\nshort line\n\n" + prose + "\n"),
        repo.write("code.rs", 'fn main() {\n    println!("hello");\n}\n'),
        repo.write("big.txt", "".join("line %d\n" % i for i in range(120))),
        repo.write("min.js", "var a=1;\n"),
    ]
    repo.commit("base", *names)

    repo.write(
        "notes.md",
        "# Notes\n\nshort line, edited\n\n"
        + prose * 3
        + "THE-END-OF-THE-PROSE-LINE\n\n"
        + "你好世界 " * 20
        + "مرحبا لا " * 20
        + "END-OF-SCRIPTS\n",
    )
    repo.write(
        "code.rs",
        "fn main() {\n"
        "    let result = some_module::some_function_with_a_long_name(argument_number_one, "
        "argument_number_two, argument_number_three).and_then(|value| "
        "another_module::transform(value, Options { verbose: true, retries: 3 }))"
        ".unwrap_or_default(); // END-OF-CODE-LINE\n"
        '    println!("hello {result:?}");\n}\n',
    )
    repo.write(
        "big.txt",
        "".join(
            "line %d: " % i + "lorem ipsum dolor sit amet " * 6 + "end-%d\n" % i
            for i in range(120)
        ),
    )
    repo.write(
        "min.js", "var a=1;" + "function f(x){return x*2};" * 400 + "/*END-OF-MINIFIED*/\n"
    )

    return [
        "Prose wraps. Select `notes.md`, press Enter. The long `+` line runs over several "
        "rows and THE-END-OF-THE-PROSE-LINE is readable; continuation rows carry a dim `+`. "
        "The Chinese and Arabic line ends in END-OF-SCRIPTS with nothing cut at the edge.",
        "Code wraps. Open `code.rs`: `// END-OF-CODE-LINE` is readable.",
        "The toggle. Press Option-z on a Mac (Alt-z elsewhere): lines clip at the edge and "
        "the ends are gone. Press it again: they are back. A plain `c` does nothing. In a "
        "terminal 154 columns wide or more the hint line also names the key, `Opt-z clip` "
        "while wrapping and `Opt-z wrap` while clipped (`Alt-z` off a Mac); `?` lists it in a "
        "terminal 97 columns wide or more.",
        "The cap. Open `min.js`: the long line stops a few rows short of the pane's bottom "
        "and ends in a dim ` … +N`. Press `v` then `y` and paste somewhere: the whole "
        "line arrives, END-OF-MINIFIED included.",
        "Paging skips nothing. Open `big.txt`, Tab to the right pane, Space a few times: "
        "each page starts where the last ended, no `line N` missing. `b` pages back.",
        "Select to the end. In `big.txt`: `v`, then End (fn-Right on a laptop): the "
        "selection reaches `end-119`. Home goes back to line 0. Esc clears it.",
        "Option-z is never an undo. Press `a` to accept a hunk (the status line says "
        "accepted), then Option-z: the wrap toggles and the accept stays. If the status "
        "line says `undid accept`, the terminal split the key into Esc then z: report it. "
        "A plain `z` should undo; that is normal.",
        "Mouse (optional). Click a continuation row of a wrapped line, then drag: the "
        "selection follows whole lines. The wheel scrolls without jumping over a line.",
    ]


def scenario_ignored(sandbox):
    """`review_ignored`: gitignored scratch files reviewed inside their repository."""
    repo = sandbox.repo("demo")
    repo.commit(
        "base",
        repo.write(".gitignore", "z_ignore_*\nz_ignore/\n"),
        repo.write("src/lib.rs", "pub fn answer() -> u32 {\n    42\n}\n"),
    )
    repo.write("z_ignore_plan.md", "# Plan\n\n- read the code\n- write the fix\n")
    repo.write("src/deep/er/z_ignore_notes.md", "notes from the agent\n")
    repo.write("z_ignore/inside.md", "a file inside an ignored folder\n")
    sandbox.config_extra(keys='review_ignored = ["z_ignore_*"]')
    demo = os.path.join(sandbox.parent, "demo")
    return [
        "Two rows, no badge. Under `demo` the list shows `src/deep/er/z_ignore_notes.md` "
        "and `z_ignore_plan.md`, each as a new file, although `.gitignore` ignores both "
        "(the config says `review_ignored = [\"z_ignore_*\"]`). `z_ignore/inside.md` is "
        "not listed: git never looks inside an ignored folder, so nothing there can be "
        "re-included.",
        "Accept. Select `z_ignore_plan.md` and press `a`: the row leaves the list.",
        "Edit. In a second terminal: `cd '%s'` then "
        "`printf -- '- run the tests\\n' >> z_ignore_plan.md`. Within a second or two the "
        "row is back as a change, one hunk with the added line." % demo,
        "Accept the edit with `a`: the row leaves again.",
        "Delete. In the second terminal: `rm z_ignore_plan.md`. The row comes back as a "
        "deletion. Press `a`: it leaves for good.",
        "The deep file is an ordinary row. Select `src/deep/er/z_ignore_notes.md`, press "
        "`A`: it leaves, and `demo` has nothing pending. `z` brings it back.",
    ]


def scenario_reload(sandbox):
    """`R` reads the config file again: a new watched folder, a skipped clone, a refusal."""
    demo = sandbox.repo("demo")
    demo.commit("base", demo.write("app.py", "def main():\n    return 1\n"))
    demo.write("app.py", "def main():\n    return 2\n")
    clone = sandbox.repo("evals-clone")
    clone.commit("base", clone.write("README.md", "a clone nobody reviews\n"))
    clone.write("README.md", "a clone nobody reviews, changed\n")
    notes = os.path.join(sandbox.parent, "notes")
    scratch = os.path.join(sandbox.parent, "scratch")
    for folder, name, text in (
        (notes, "todo.md", "- ship it\n"),
        (scratch, "idea.md", "an idea\n"),
    ):
        os.makedirs(folder)
        with open(os.path.join(folder, name), "w", encoding="utf-8") as f:
            f.write(text)
    sandbox.config_extra(keys='draft_dirs = [\n  "notes",\n  # "scratch",\n]')
    config = sandbox.config
    return [
        "Three roots. The list shows `demo` (one change, `app.py`), `evals-clone` (one "
        "change) and `parent/notes` (nothing pending). `parent/scratch` is not listed: its "
        "line in the "
        "config is commented out.",
        "Accept something. Select `app.py` under `demo` and press `A`: the row leaves and "
        "`demo` has nothing pending.",
        "Uncomment the second watched folder. In a second terminal open `%s` in an editor, "
        "delete the `# ` in front of `\"scratch\",` and save. The screen does not change: "
        "nothing reads the file until you ask." % config,
        "Press `R`. The status line says `config reloaded: 1 root added` and `parent/scratch` is "
        "listed, with nothing pending (`draft_initial` is `seen`). `demo` still has nothing "
        "pending: the accept from step 2 is intact. Edit `%s/idea.md` in the second "
        "terminal and it shows up as a change." % scratch,
        "Skip a repository. Add the line `skip_globs = [\"evals-clone\"]` at the very top "
        "of the config file (above `[update]`: a top-level key cannot follow a table), save, "
        "press `R`. The status line says `config reloaded: 1 root removed, skip_globs` and "
        "`evals-clone` leaves the list. Delete the line and press `R` again: it is back.",
        "Break the file. Add a last line `this is not toml` and save, then press `R`. The "
        "status line says `config not reloaded:` then the line number and what is wrong. "
        "Nothing on the screen changed and every key still works: `r` rescans and says "
        "`refreshed`.",
        "Mend it. Remove the bad line, press `R`: `config reloaded, nothing changed`.",
        "The key. `?` lists `R  reload the config file` next to `r`. In a terminal wide "
        "enough, the hint line at the bottom names `R reload` after `r refresh`; it is the "
        "first hint to go when the line is short.",
    ]


def scenario_cherry_pick(sandbox):
    """Content already accepted on another branch folds into one `seen` row, and opens."""
    repo = sandbox.repo("demo")
    # The person runs git by hand in this repository, so its own config carries what their
    # global config might not have (an identity) or might add (signing, hooks).
    for key, value in (
        ("user.name", "tryout"),
        ("user.email", "tryout@example.invalid"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ):
        repo.git("config", key, value)
    repo.commit(
        "base",
        repo.write("a.rs", "pub fn a() -> u32 {\n    1\n}\n"),
        repo.write("b.rs", "pub fn b() -> u32 {\n    2\n}\n"),
        repo.write("c.rs", "pub fn c() -> u32 {\n    3\n}\n"),
    )
    for branch in ("feat-x", "feat-y", "feat-z"):
        repo.git("branch", branch)
    repo.git("switch", "-q", "-c", "run-2")
    repo.commit(
        "run-2 work",
        repo.write("d.rs", "pub fn d() -> u32 {\n    4\n}\n"),
        repo.write("e.rs", "pub fn e() -> u32 {\n    5\n}\n"),
    )
    repo.git("switch", "-q", "main")
    repo.git("switch", "-q", "-c", "run-1")
    for name, n in (("a.rs", 10), ("b.rs", 20), ("c.rs", 30)):
        stem = name[0]
        repo.write(
            name,
            "pub fn %s() -> u32 {\n    %d\n}\n\npub fn %s_twice() -> u32 {\n    %s() * 2\n}\n"
            % (stem, n, stem, stem),
        )
    demo = os.path.join(sandbox.parent, "demo")
    status = "LASTCALL_STATE_DIR='%s' LASTCALL_CONFIG='%s' '%s' status" % (
        sandbox.state,
        sandbox.config,
        BINARY,
    )
    return [
        "The reviewed work. Keep lastcall open and use a second terminal for git: "
        "`cd '%s'`. The list shows `demo` on `run-1 · 3 files`: `a.rs`, `b.rs`, `c.rs`, "
        "each edited. Read them, then press `ctrl-a`: the list empties. In the second "
        "terminal: `git commit -qam \"run-1 work\"`. The list stays empty." % demo,
        "The switch. `git switch feat-x`: the branch line says `feat-x` and the list stays "
        "empty (the status line says it is your first time on feat-x, carried over from "
        "run-1).",
        "The cherry-pick. `git cherry-pick run-1`: within a second or two the list shows "
        "one row, `seen · 3 files`, not three rows.",
        "Read it. Select `seen · 3 files`: the right pane says `3 files, content accepted "
        "on run-1` and lists the three paths, each with `run-1` beside it.",
        "Open it. Press `e`: `a.rs`, `b.rs` and `c.rs` appear indented under the group "
        "row, each badged `[seen]`; once the `HEAD moved` status line has cleared (it stays "
        "for half a minute) the hint line says `e collapse`. Select `b.rs`: its diff shows "
        "like any row's, `[seen]` on its header. Press `e` again: they fold back and the "
        "group row is selected.",
        "One file changes. `printf 'extra\\n' >> b.rs`: `b.rs` becomes its own row above "
        "`seen · 2 files`, with no badge (its content is new).",
        "Flag a member. Press `e` on the group, select `c.rs`, press `m`, type a note, "
        "press Enter: `c.rs` leaves the group as its own row with the flag mark and "
        "`[seen]`, and `seen · 1 file` remains, still open with `a.rs` under it.",
        "Accept the group. Select `seen · 1 file`, press `a`: refused, the status line "
        "says `A accepts the group`. Press `A`: `accepted seen · 1 file` and the group "
        "row is gone. Press `z`: `undid accept of a.rs` and `seen · 1 file` is back.",
        "Accept all. In the second terminal make ten scratch files so `ctrl-a` asks first: "
        "`for i in 1 2 3 4 5 6 7 8 9 10; do echo \"note $i\" > note-$i.txt; done`. Press "
        "`ctrl-a`: the modal says `Accept all 13 files in demo?` and `0 grouped upstream "
        "· 1 grouped seen · 0 collapsed`: every pending file, folded or not. Press `y`: "
        "the list empties. Then commit so the next switch starts clean: "
        "`git add -A && git commit -qm \"feat-x work\"`.",
        "The rebase variant. `git switch run-2`: two rows, `d.rs` and `e.rs`, no group. "
        "Press `ctrl-a`: empty. `git switch feat-y`: empty. `git rebase run-2`: the "
        "list shows `seen · 2 files`, and selecting it says `content accepted on run-2`.",
        "The squash-merge variant. `git switch feat-z`: empty. `git merge --squash run-2`: "
        "`seen · 2 files` again.",
        "Nothing hidden. In the second terminal: `%s`. It lists `d.rs` and `e.rs` each "
        "with `[seen]`, then the line `seen · 2 files`. Add `--json` to the same command: "
        "each pending row carries a `seen_on` list holding `run-2` and `groups` holds one "
        "`\"kind\": \"seen\"`." % status,
    ]


SCENARIOS = {
    "cherry-pick": scenario_cherry_pick,
    "ignored": scenario_ignored,
    "reload": scenario_reload,
    "wrap": scenario_wrap,
}


# ---- the runner ------------------------------------------------------------------------


# `--check` answers "is this a scenario" and builds nothing: the just recipe asks before it
# pays for a release build.
ALLOWED_FLAGS = ("--no-launch", "--check")


def first_line(doc):
    return (doc or "").strip().splitlines()[0]


def build(name, launch_in=None):
    sandbox = Sandbox(name)
    steps = SCENARIOS[name](sandbox)
    if launch_in is not None:
        # With `parent_dirs` naming parent/, opening from inside it changes nothing: the
        # launch directory decides what is watched only when the config names none.
        sandbox.launch_in = launch_in
        sandbox.name_parent = False
    try:
        launch_dir = sandbox.launch_dir()
    except ValueError as err:
        raise ValueError("%s (built so far: %s)" % (err, sandbox.base))
    sandbox.write_config()

    text = ["# lastcall tryout: %s" % name, "", first_line(SCENARIOS[name].__doc__), ""]
    text += ["%d. %s" % (i, step) for i, step in enumerate(steps, 1)]
    text += ["", "`q` quits. Run it once outside a herdr pane and once inside one.", ""]
    with open(os.path.join(sandbox.base, "STEPS.md"), "w", encoding="utf-8") as f:
        f.write("\n".join(text))

    with open(os.path.join(sandbox.base, "run.py"), "w", encoding="utf-8") as f:
        f.write(
            "import os\n"
            "os.environ['LASTCALL_STATE_DIR'] = %r\n"
            "os.environ['LASTCALL_CONFIG'] = %r\n"
            "os.chdir(%r)\n"
            "os.execv(%r, ['lastcall', 'tui'])\n"
            % (sandbox.state, sandbox.config, launch_dir, BINARY)
        )
    return sandbox, "\n".join(text)


def earlier_sandboxes(name, base):
    """The other `lastcall-tryout-<name>-*` directories beside `base`, newest first.

    A person who runs a scenario twice has two sandboxes whose paths differ by a suffix,
    and a lastcall opened by the first run keeps watching the first one; the git commands
    in the second run's steps then land where nothing is looking. The note that lists
    them is the difference between a puzzling "nothing changes" and a `cd` into the right
    directory.
    """
    prefix = "lastcall-tryout-%s-" % name
    tmp = os.path.dirname(base)
    found = []
    for entry in os.listdir(tmp):
        path = os.path.join(tmp, entry)
        if entry.startswith(prefix) and path != base and os.path.isdir(path):
            steps = os.path.join(path, "STEPS.md")
            built = os.path.getmtime(steps if os.path.exists(steps) else path)
            found.append((built, path))
    found.sort(reverse=True)
    return found


def main(argv):
    args = [a for a in argv if not a.startswith("--")]
    flags = [a for a in argv if a.startswith("--")]
    if args == ["list"] or not args:
        for name in sorted(SCENARIOS):
            print("%-12s %s" % (name, first_line(SCENARIOS[name].__doc__)))
        return 0 if args else 2
    name = args[0]
    launch_in = None
    for flag in [f for f in flags if f.startswith("--in=")]:
        launch_in = flag[len("--in="):]
        flags.remove(flag)
    if len(args) != 1 or name not in SCENARIOS or any(f not in ALLOWED_FLAGS for f in flags):
        print("usage: tryout.py list | <scenario> [--no-launch] [--in=<dir>]", file=sys.stderr)
        print("scenarios: " + ", ".join(sorted(SCENARIOS)), file=sys.stderr)
        return 2
    if "--check" in flags:
        return 0
    if not os.path.exists(BINARY):
        print("no %s: run `just tryout %s`, which builds it" % (BINARY, name), file=sys.stderr)
        return 2

    try:
        sandbox, steps = build(name, launch_in)
    except ValueError as err:
        print("tryout: %s" % err, file=sys.stderr)
        return 2
    run = os.path.join(sandbox.base, "run.py")
    print(steps)
    print("sandbox: %s" % sandbox.base)
    print("opens in: %s" % os.path.join(sandbox.parent, sandbox.launch_in).rstrip(os.sep))
    print("steps:   %s" % os.path.join(sandbox.base, "STEPS.md"))
    print("reopen:  python3 '%s'" % run)
    others = earlier_sandboxes(name, sandbox.base)
    if others:
        built, newest = others[0]
        print(
            "note:    %d earlier %s sandbox%s here, newest built %s: %s. The steps and "
            "the git commands above name this run's sandbox; a lastcall opened by an "
            "earlier run is still watching that run's."
            % (
                len(others),
                name,
                "" if len(others) == 1 else "es",
                time.strftime("%H:%M", time.localtime(built)),
                newest,
            )
        )
    if "--no-launch" in flags:
        return 0
    try:
        input("\nEnter opens lastcall over the sandbox (Ctrl-C to stop here): ")
    except (KeyboardInterrupt, EOFError):
        print()
        return 0
    os.execv(sys.executable, [sys.executable, run])


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
