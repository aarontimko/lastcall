#!/usr/bin/env python3
"""The hand-typed parts of a release, as three verbs.

Nothing here changes what a release is (docs/dev/operations.md, "Cutting a release"):
`main` still takes every change through a pull request with the checks green, the tag is
still an annotated `v<crate version>` on the merge commit, and `release.yml` still builds
from the tag. The verbs replace the run of commands typed at each step, and refuse where a
person would have had to notice something by eye.

    release.py prep <version>    from an up-to-date `main`: the branch `release/v<version>`
                                 and its one commit. For a release that is five files: the
                                 crate version, the lockfile, the dated CHANGELOG heading,
                                 and the version README.md and docs/install.md name. A
                                 candidate (`0.4.0-rc.1`) leaves the two install pages
                                 alone. Pushes nothing.
    release.py merge [number]    any pull request, release or not: wait for the required
                                 checks, one yes, a merge commit, then local `main` pulled
                                 and the merged branch deleted.
    release.py tag               on the merged `main`: refuse what `release.yml` would
                                 refuse after the builds, one yes, the annotated tag, its
                                 push, the release run watched to the end.

`merge` and `tag` send to GitHub, so they are the maintainer's: they refuse inside a coding
agent's shell (operations.md, "The maintainer pushes and tags; agents do not"). `prep` is
anyone's.

RELEASE_DRY_RUN=1 runs every check and prints, instead of running, each command that would
write a branch, a commit or a tag or leave the machine. It is allowed in an agent's shell.

Standard library only, and nothing newer than Python 3.9 (what macOS ships).
"""

import datetime
import json
import os
import re
import shlex
import subprocess
import sys
import time

DRY = os.environ.get("RELEASE_DRY_RUN") == "1"
VERSION = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-rc\.([1-9][0-9]*))?$")
INSTALL_PAGES = ("README.md", "docs/install.md")


def say(message):
    print("release: " + message, file=sys.stderr)


def die(message):
    say(message)
    sys.exit(1)


def ok(*cmd):
    """Did a read-only command succeed? Its output is thrown away."""
    return subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0


def out(*cmd, why=None):
    """The stdout of a read-only command; the run stops when the command fails."""
    done = subprocess.run(cmd, stdout=subprocess.PIPE, universal_newlines=True)
    if done.returncode != 0:
        die(why or "failed: " + " ".join(shlex.quote(c) for c in cmd))
    return done.stdout.strip()


def gh_json(*cmd, why=None):
    text = out("gh", *cmd, why=why)
    try:
        return json.loads(text)
    except ValueError:
        die("gh %s did not answer in JSON: %s" % (" ".join(cmd[:2]), text[:200]))


def run(*cmd):
    """Run a command that changes something, or print it under RELEASE_DRY_RUN=1."""
    if DRY:
        say("would run: " + " ".join(shlex.quote(c) for c in cmd))
        return True
    return subprocess.run(cmd).returncode == 0


def must(*cmd):
    if not run(*cmd):
        die("failed: " + " ".join(shlex.quote(c) for c in cmd))


def by_hand(verb):
    # CLAUDECODE is what one agent harness sets; the rule itself is in operations.md and
    # binds every agent, whether or not this line can see it.
    if "CLAUDECODE" in os.environ and not DRY:
        die("'%s' sends to GitHub, so a person runs it, not an agent's shell "
            "(RELEASE_DRY_RUN=1 is allowed)" % verb)


def confirm(question):
    if DRY:
        return
    if not sys.stdin.isatty():
        die("no terminal to confirm on: " + question)
    try:
        answer = input("release: %s [y/N] " % question)
    except EOFError:
        answer = ""
    if answer.strip() not in ("y", "Y"):
        die("stopped at your word; nothing was sent")


def branch():
    return out("git", "symbolic-ref", "--quiet", "--short", "HEAD", why="detached HEAD")


def clean():
    if out("git", "status", "--porcelain"):
        die("the working tree has changes; commit or set them aside first")


def fetch():
    # Reads from GitHub and moves only the remote-tracking refs, so a dry run does it too.
    if subprocess.run(["git", "fetch", "--quiet", "--tags", "origin", "main"]).returncode != 0:
        die("could not fetch origin")


def at_origin_main():
    fetch()
    head, origin = out("git", "rev-parse", "HEAD", "origin/main").split()
    if head != origin:
        die("main is not at origin/main; pull first")
    return head


def read(path):
    with open(path, encoding="utf-8") as handle:
        return handle.read()


def write(path, text):
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)


def crate_version(cargo_toml):
    """`[workspace.package].version`, the one line release.yml reads too."""
    found = re.findall(r'^version = "([^"]*)"$', cargo_toml, flags=re.M)
    if len(found) != 1:
        die("expected exactly one top-level version line in Cargo.toml, found %d" % len(found))
    return found[0]


def changelog_section(changelog, name):
    """The body of the section headed `## <name>`, then a space or the end of the line,
    matched as release.yml matches it. None when there is no such section."""
    body = None
    for line in changelog.splitlines():
        if line.startswith("## "):
            if body is not None:
                break
            if line == "## " + name or line.startswith("## " + name + " "):
                body = []
        elif body is not None and line.strip():
            body.append(line)
    return body


def parse(version):
    hit = VERSION.match(version)
    if not hit:
        return None
    major, minor, patch, rc = hit.groups()
    # On one base a candidate precedes its release, and rc.1 precedes rc.2.
    return (int(major), int(minor), int(patch), 0 if rc else 1, int(rc or 0))


def names(version):
    """A version as a whole token (`v0.3.0`, `version=0.3.0`), never as part of another
    (`10.3.0`, `0.3.0-rc.1`, `0.3.01`)."""
    return re.compile(r"(?<![0-9.])" + re.escape(version) + r"(?![0-9-]|\.[0-9])")


def cmd_prep(args):
    new = args[0] if len(args) == 1 else ""
    if not parse(new):
        die("usage: prep <version>, like 0.4.0 or 0.4.0-rc.1 (no leading v, no leading zeros)")
    if branch() != "main":
        die("start from main")
    clean()
    at_origin_main()

    cargo_toml = read("Cargo.toml")
    old = crate_version(cargo_toml)
    if parse(old) is None or parse(new) <= parse(old):
        die("%s is not later than the crate's %s" % (new, old))
    if ok("git", "rev-parse", "--quiet", "--verify", "refs/tags/v" + new):
        die("the tag v%s exists" % new)
    release_branch = "release/v" + new
    if ok("git", "show-ref", "--verify", "--quiet", "refs/heads/" + release_branch):
        die("the branch %s exists" % release_branch)

    # Every file's new text is worked out, and every refusal made, before the first write:
    # a refusal leaves nothing to clean up.
    base = new.split("-")[0]
    final = base == new
    changes = {"Cargo.toml": re.sub(r'^version = "[^"]*"$', 'version = "%s"' % new,
                                    cargo_toml, count=1, flags=re.M)}

    changelog = read("CHANGELOG.md")
    heading = "## %s - %s" % (base, datetime.date.today().isoformat())
    dated = changelog_section(changelog, base)
    unreleased = changelog_section(changelog, "Unreleased")
    if dated is not None:
        # A candidate already dated this section. Notes written since belong inside it, and
        # the release takes today's date.
        if unreleased is not None:
            die("CHANGELOG.md has both '## %s' and '## Unreleased'; fold the second into the first" % base)
        if not dated:
            die("the '## %s' section of CHANGELOG.md is empty" % base)
        pattern = r"^## " + re.escape(base) + r"( .*)?$"
    else:
        if unreleased is None:
            die("CHANGELOG.md has neither a '## %s' section nor '## Unreleased'" % base)
        if not unreleased:
            # The section is the release page: refuse to date an empty one.
            die("'## Unreleased' in CHANGELOG.md is empty; the release notes are written first")
        pattern = r"^## Unreleased$"
    changes["CHANGELOG.md"] = re.sub(pattern, heading, changelog, count=1, flags=re.M)

    shown = None
    if final:
        # The install pages name the last release, which after a candidate is not the
        # crate's version. docs/install.md's `version=` line says which one they name.
        hit = re.search(r"^version=([0-9]+\.[0-9]+\.[0-9]+)$", read("docs/install.md"), flags=re.M)
        if not hit:
            die("docs/install.md has no 'version=<x.y.z>' line; the install steps moved, edit this script")
        shown = hit.group(1)
        for page in INSTALL_PAGES:
            text = read(page)
            count = len(names(shown).findall(text))
            if count != 2:
                die("%s names %s %d times, expected 2; look, then edit this script" % (page, shown, count))
            changes[page] = names(shown).sub(new, text)

    must("git", "switch", "-c", release_branch)
    if DRY:
        say("would write %s -> %s into %s, and the three workspace crates into Cargo.lock"
            % (old, new, ", ".join(sorted(changes))))
        return
    undo = "git restore . && git switch main && git branch -D " + release_branch
    try:
        for path, text in changes.items():
            write(path, text)
        if not ok("cargo", "update", "--workspace", "--offline"):
            must("cargo", "update", "--workspace")
        # The lockfile may move by the three workspace crates and nothing else.
        moved = out("git", "diff", "--numstat", "Cargo.lock").split()[:2]
        if moved != ["3", "3"]:
            die("Cargo.lock moved by %s lines, expected 3/3" % "/".join(moved or ["0", "0"]))
        body = "The changelog section is dated and the workspace version is %s" % new
        body += "; the two install documents name v%s." % new if final else "."
        must("git", "add", "Cargo.toml", "Cargo.lock", "CHANGELOG.md", *(INSTALL_PAGES if final else ()))
        must("git", "commit", "--quiet", "-m", "chore(release): " + new, "-m", body)
    except SystemExit:
        say("stopped halfway, on %s. To undo: %s" % (release_branch, undo))
        raise
    subprocess.run(["git", "--no-pager", "show", "-U0", "--format=%h %s%n", "HEAD", "--",
                    "Cargo.toml", "CHANGELOG.md", *INSTALL_PAGES])
    say("every changed line is above (Cargo.lock: the three workspace crates)")
    say("next: push %s, open its pull request, then: just merge" % release_branch)


def cmd_merge(args):
    by_hand("merge")
    if len(args) > 1:
        die("usage: merge [pull request number]")
    target = args[0] if args else branch()
    if target == "main":
        die("on main; name the pull request: just merge <number>")
    fields = "number,title,state,headRefName,headRefOid,baseRefName,isCrossRepository"
    pr = gh_json("pr", "view", target, "--json", fields, why="no pull request found for " + target)
    number, head = str(pr["number"]), pr["headRefName"]
    if pr["state"] != "OPEN":
        die("#%s is %s" % (number, pr["state"]))
    if pr["baseRefName"] != "main":
        die("#%s goes into %s, not main" % (number, pr["baseRefName"]))
    clean()

    # Both local refusals come before the merge: after it, nothing is left that can fail
    # except the network.
    fetch()
    if not ok("git", "merge-base", "--is-ancestor", "main", "origin/main"):
        die("local main has commits origin/main lacks; it could not be pulled after the merge")
    local = "refs/heads/" + head
    has_local = not pr["isCrossRepository"] and ok("git", "show-ref", "--verify", "--quiet", local)
    if has_local and out("git", "rev-parse", local) != pr["headRefOid"]:
        die("local %s is not what #%s holds (%s); push it, or look, before merging"
            % (head, number, pr["headRefOid"][:7]))

    # Required checks only: the advisory macOS leg may be red on a pull request GitHub
    # itself would merge.
    say("#%s \"%s\": waiting for the required checks" % (number, pr["title"]))
    if subprocess.run(["gh", "pr", "checks", number, "--required", "--watch", "--interval", "15"]).returncode != 0:
        die("the required checks on #%s are not green" % number)
    confirm("merge #%s \"%s\" into main?" % (number, pr["title"]))
    # GitHub refuses when the head moved while the question sat on screen.
    must("gh", "pr", "merge", number, "--merge", "--match-head-commit", pr["headRefOid"])
    if not DRY:
        say("merged #%s on GitHub; what follows is local" % number)

    must("git", "switch", "main")
    must("git", "pull", "--ff-only", "origin", "main")
    if has_local:
        must("git", "branch", "-d", head)
    must("git", "fetch", "--prune", "origin")
    if not DRY:
        say("main is at " + out("git", "log", "-1", "--format=%h %s"))
        if head.startswith("release/v"):
            say("next: just release-tag")


def ci_runs(sha):
    return gh_json("run", "list", "--commit", sha, "--workflow", "ci.yml",
                   "--json", "databaseId,status,conclusion")


def cmd_tag(args):
    by_hand("tag")
    if args:
        die("usage: tag (the version comes from Cargo.toml)")
    if branch() != "main":
        die("tags are made on main")
    clean()
    sha = at_origin_main()
    version = crate_version(read("Cargo.toml"))
    base = version.split("-")[0]
    tag = "v" + version

    if out("git", "ls-remote", "--tags", "origin", "refs/tags/" + tag):
        die("%s is on GitHub already: the crate version was not bumped, or this release is cut" % tag)
    reuse = ok("git", "rev-parse", "--quiet", "--verify", "refs/tags/" + tag)
    if reuse:
        # A local tag that never reached GitHub: an earlier run whose push did not finish.
        if out("git", "rev-parse", tag + "^{commit}") != sha:
            die("a local %s sits on another commit and was never pushed; "
                "remove it (git tag -d %s) and run this again" % (tag, tag))
        say("reusing the local %s: it is on this commit and not on GitHub" % tag)

    # release.yml takes the notes from `## <version>`, then `## <base>` for a candidate, and
    # fails after the four builds when it finds no body.
    changelog = read("CHANGELOG.md")
    if not (changelog_section(changelog, version) or changelog_section(changelog, base)):
        die("CHANGELOG.md has no '## %s' section with a body; release.yml would fail after the builds" % base)
    if changelog_section(changelog, "Unreleased") is not None:
        die("CHANGELOG.md still has '## Unreleased': the release commit is not on main")

    # The ci workflow ran on this commit and passed. The weekly jobs are early warnings
    # (operations.md) and do not gate a release.
    runs = ci_runs(sha)
    if not runs:
        die("no ci run found for %s; is this the merge commit?" % sha[:7])
    for pending in [r for r in runs if r["status"] != "completed"]:
        say("ci is still running on this commit; watching it")
        subprocess.run(["gh", "run", "watch", str(pending["databaseId"]), "--interval", "20"])
        runs = ci_runs(sha)
    if any(r["status"] != "completed" or r["conclusion"] != "success" for r in runs):
        die("ci did not pass on this commit")

    done = subprocess.run(["git", "describe", "--tags", "--abbrev=0", "--match", "v*", "--exclude", tag],
                          stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, universal_newlines=True)
    previous = done.stdout.strip() if done.returncode == 0 else ""
    say("%s on %s; previous tag %s; ci green"
        % (tag, out("git", "log", "-1", "--format=%h %s"), previous or "none"))
    confirm("tag and publish %s? A published release is not taken back" % tag)
    if not reuse:
        must("git", "tag", "-a", tag, "-m", tag)
    say("pushing the tag (the pre-push hook runs the integration tier first; let it finish)")
    if not run("git", "push", "origin", tag):
        # Leave nothing behind that a bare push of the tag could publish unchecked.
        subprocess.run(["git", "tag", "-d", tag], stdout=subprocess.DEVNULL)
        die("the push did not go through and the local %s is removed; fix the cause and run this again" % tag)
    if DRY:
        return

    run_id = ""
    for _ in range(24):
        found = gh_json("run", "list", "--workflow", "release.yml", "--branch", tag,
                        "--limit", "1", "--json", "databaseId")
        if found:
            run_id = str(found[0]["databaseId"])
            break
        time.sleep(5)
    if not run_id:
        die("release.yml did not start for %s after two minutes; look at the Actions page" % tag)
    if subprocess.run(["gh", "run", "watch", run_id, "--exit-status", "--interval", "20"]).returncode != 0:
        die("the release run failed: gh run view %s --log-failed" % run_id)
    subprocess.run(["gh", "release", "view", tag, "--json", "url,assets",
                    "--jq", '.url, (.assets[] | "  " + .name)'])
    say("published. Step 4 of operations.md is left: the checksums, the attestations"
        + (", and:" if previous else ""))
    if previous:
        say("  just install-smoke %s %s" % (previous, tag))


def main():
    verbs = {"prep": cmd_prep, "merge": cmd_merge, "tag": cmd_tag}
    if len(sys.argv) < 2 or sys.argv[1] not in verbs:
        die("usage: release.py prep <version> | merge [number] | tag")
    top = subprocess.run(["git", "rev-parse", "--show-toplevel"], stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL, universal_newlines=True)
    if top.returncode != 0:
        die("not inside the repository")
    os.chdir(top.stdout.strip())
    try:
        # `just` passes an optional argument left out as an empty string.
        verbs[sys.argv[1]]([a for a in sys.argv[2:] if a])
    except KeyboardInterrupt:
        die("interrupted")


if __name__ == "__main__":
    main()
