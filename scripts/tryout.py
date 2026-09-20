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

import os
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BINARY = os.path.join(ROOT, "target", "release", "lastcall")


class Repo:
    """One git repository under the sandbox's parent directory."""

    def __init__(self, path):
        self.path = path
        os.makedirs(path)
        self.git("init", "-q", "-b", "main", ".")

    def git(self, *args):
        # The identity is the commit's own, so the person's git config is neither needed
        # nor changed; the hooks path is emptied so a global hook cannot run here.
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

    def repo(self, name):
        return Repo(os.path.join(self.parent, name))

    def config_extra(self, keys="", tables=""):
        """Top-level `key = value` lines and whole `[table]` blocks for config.toml."""
        if keys:
            self.config_keys.append(keys.strip("\n"))
        if tables:
            self.config_tables.append(tables.strip("\n"))

    def write_config(self):
        # The daily update check is off: a hands-on run makes no network request.
        lines = ['parent_dirs = ["%s"]' % self.parent]
        lines += self.config_keys
        lines += ["", "[update]", "check = false"]
        for table in self.config_tables:
            lines += ["", table]
        with open(self.config, "w", encoding="utf-8") as f:
            f.write("\n".join(lines) + "\n")


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
        "The toggle. Press `c`: lines clip at the edge and the hint at the bottom changes "
        "between `c clip` and `c wrap`. Press `c` again. Then Option-z on a Mac (Alt-z "
        "elsewhere): it does the same.",
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


SCENARIOS = {
    "wrap": scenario_wrap,
}


# ---- the runner ------------------------------------------------------------------------


def first_line(doc):
    return (doc or "").strip().splitlines()[0]


def build(name):
    sandbox = Sandbox(name)
    steps = SCENARIOS[name](sandbox)
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
            % (sandbox.state, sandbox.config, sandbox.parent, BINARY)
        )
    return sandbox, "\n".join(text)


def main(argv):
    args = [a for a in argv if not a.startswith("--")]
    flags = [a for a in argv if a.startswith("--")]
    if args == ["list"] or not args:
        for name in sorted(SCENARIOS):
            print("%-12s %s" % (name, first_line(SCENARIOS[name].__doc__)))
        return 0 if args else 2
    name = args[0]
    if len(args) != 1 or name not in SCENARIOS or any(f != "--no-launch" for f in flags):
        print("usage: tryout.py list | <scenario> [--no-launch]", file=sys.stderr)
        print("scenarios: " + ", ".join(sorted(SCENARIOS)), file=sys.stderr)
        return 2
    if not os.path.exists(BINARY):
        print("no %s: run `just tryout %s`, which builds it" % (BINARY, name), file=sys.stderr)
        return 2

    sandbox, steps = build(name)
    run = os.path.join(sandbox.base, "run.py")
    print(steps)
    print("sandbox: %s" % sandbox.base)
    print("steps:   %s" % os.path.join(sandbox.base, "STEPS.md"))
    print("reopen:  python3 %s" % run)
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
