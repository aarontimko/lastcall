#!/usr/bin/env python3
"""The hand-typed parts of a release, as three verbs.

Nothing here changes what a release is (docs/dev/operations.md, "Cutting a release"):
`main` still takes every change through a pull request with the checks green, the tag is
still an annotated `v<crate version>` on a merge commit, and `release.yml` still builds
from the tag. The verbs replace the run of commands typed at each step, and refuse where a
person would have had to notice something by eye.

A release is one pull request: the branch carrying the change also carries the version
bump as its last commit, and the tag goes on that pull request's merge commit.

    release.py prep <version>    on the branch whose pull request carries the release (any
                                 branch but `main`), clean and holding all of origin/main:
                                 one commit on that branch. For a release that is five
                                 files: the crate version, the lockfile, the dated CHANGELOG
                                 heading, and the version README.md and docs/install.md
                                 name. A candidate (`0.4.0-rc.1`) leaves the two install
                                 pages alone. Refuses a version not later than the crate's
                                 and than every `v*` tag on origin. Pushes nothing.
    release.py merge [number]    any pull request, release or not: wait for the required
                                 checks, one yes, a merge commit, then local `main` pulled
                                 and the merged branch deleted. When the merge carried a
                                 bump, it says so and names the tag step.
    release.py tag [commit]      on the merged `main`: the commit (main's tip unless named)
                                 must be on origin/main and be the merge commit whose
                                 first-parent diff carries the bump, so nothing merged
                                 after it ships under its number. Then what `release.yml`
                                 would refuse after the builds, one yes, the annotated tag
                                 on that commit, its push, the release run watched to the
                                 end. Name the commit when something merged after it.
    release.py self-test         the rules above over made-up text, with no repository and
                                 no network; `just lint` runs it.

`merge` and `tag` send to GitHub, so they are the maintainer's: they refuse inside a coding
agent's shell (operations.md, "The maintainer pushes and tags; agents do not"). `prep` is
anyone's.

RELEASE_DRY_RUN=1 runs every check and prints, instead of running, each command that would
write a file, a commit or a tag or leave the machine. It is allowed in an agent's shell.

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


class Refusal(Exception):
    """A rule said no. The pure helpers below raise it, so the self-test can ask them
    without a repository; `main` prints it and exits 1, as `die` does."""


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
        raise Refusal("expected exactly one top-level version line in Cargo.toml, found %d" % len(found))
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


def later(new, old):
    """Is `new` a later version than `old`? A candidate precedes its release."""
    return parse(new) is not None and parse(old) is not None and parse(new) > parse(old)


def check_prep_branch(name):
    """The branch `prep` commits the bump on: the one whose pull request carries the
    release, never `main`, which takes changes only through a pull request."""
    if name == "main":
        raise Refusal("prep commits the bump on the branch whose pull request carries the "
                      "release, never on main; switch to that branch (or make a new one for "
                      "the final after a candidate)")


def date_changelog(changelog, new, today):
    """CHANGELOG.md with the release's heading dated: `## Unreleased` becomes
    `## <base> - <today>`, or, after a candidate dated the section already, that heading
    takes today's date. Refuses an empty section, and a file with both or neither."""
    base = new.split("-")[0]
    heading = "## %s - %s" % (base, today)
    dated = changelog_section(changelog, base)
    unreleased = changelog_section(changelog, "Unreleased")
    if dated is not None:
        # A candidate already dated this section. Notes written since belong inside it, and
        # the release takes today's date.
        if unreleased is not None:
            raise Refusal("CHANGELOG.md has both '## %s' and '## Unreleased'; fold the second into the first" % base)
        if not dated:
            raise Refusal("the '## %s' section of CHANGELOG.md is empty" % base)
        pattern = r"^## " + re.escape(base) + r"( .*)?$"
    else:
        if unreleased is None:
            raise Refusal("CHANGELOG.md has neither a '## %s' section nor '## Unreleased'" % base)
        if not unreleased:
            # The section is the release page: refuse to date an empty one.
            raise Refusal("'## Unreleased' in CHANGELOG.md is empty; the release notes are written first")
        pattern = r"^## Unreleased$"
    return re.sub(pattern, heading, changelog, count=1, flags=re.M)


def shown_version(install_md):
    """The release the install pages name: docs/install.md's `version=` line."""
    hit = re.search(r"^version=([0-9]+\.[0-9]+\.[0-9]+)$", install_md, flags=re.M)
    if not hit:
        raise Refusal("docs/install.md has no 'version=<x.y.z>' line; the install steps moved, edit this script")
    return hit.group(1)


def name_new_version(page, text, shown, new):
    """An install page naming `new` where it named `shown`, which it must name exactly
    twice, as whole tokens."""
    count = len(names(shown).findall(text))
    if count != 2:
        raise Refusal("%s names %s %d times, expected 2; look, then edit this script" % (page, shown, count))
    return names(shown).sub(new, text)


def remote_versions(listing):
    """The versions of the `v*` tags in `git ls-remote --tags` output."""
    found = set()
    for line in listing.splitlines():
        ref = line.split("\t")[-1].strip()
        if ref.endswith("^{}"):
            ref = ref[:-3]
        name = ref[len("refs/tags/"):] if ref.startswith("refs/tags/") else ""
        if name.startswith("v") and parse(name[1:]):
            found.add(name[1:])
    return found


def check_later(new, crate, listing):
    """`new` must be later than the crate's version and than the newest `v*` tag in
    `listing` (`git ls-remote --tags` output): a version is never released twice, and
    never below one already out."""
    if not later(new, crate):
        raise Refusal("%s is not later than the crate's %s" % (new, crate))
    released = remote_versions(listing)
    if released:
        newest = max(released, key=parse)
        if not later(new, newest):
            raise Refusal("%s is not later than v%s, the newest tag on origin" % (new, newest))


def check_tag_absent(new, listing, local):
    """The tag `v<new>` must be neither on origin (`listing`) nor in this clone."""
    if new in remote_versions(listing):
        raise Refusal("the tag v%s is on origin already" % new)
    if local:
        raise Refusal("the tag v%s exists locally; if it was never pushed, remove it (git tag -d v%s)"
                      % (new, new))


def check_release_merge(parents, cargo_diff, version):
    """The commit tag takes: a merge commit (two parents) whose first-parent diff of
    Cargo.toml (`git diff <c>^1 <c> -- Cargo.toml`) sets the `[workspace.package]` version
    line to `version`, the crate version at that commit. That is the merge that brought the
    bump, and not a merge made after it, whose own diff would not carry the bump.

    The version line is the file's one unindented `version = "..."` line, which
    crate_version has already required to be unique, so in a diff it is the line that
    begins `-version = ` or `+version = `."""
    if parents != 2:
        raise Refusal("not a merge commit (%d parent%s): the tag goes on the merge of the pull "
                      "request that carried the bump" % (parents, "" if parents == 1 else "s"))
    removed = re.findall(r'^-version = "([^"]*)"$', cargo_diff, flags=re.M)
    added = re.findall(r'^\+version = "([^"]*)"$', cargo_diff, flags=re.M)
    if len(removed) != 1 or added != [version] or removed == added:
        raise Refusal("a merge, but its first-parent diff does not change Cargo.toml's version "
                      "line to %s: this is not the merge that carried the bump" % version)

def check_tag_notes(changelog, version):
    """What release.yml would fail on after the four builds, and a sign the commit is not
    the release: no section with a body for the version, or `## Unreleased` still there."""
    base = version.split("-")[0]
    # release.yml takes the notes from `## <version>`, then `## <base>` for a candidate, and
    # fails after the four builds when it finds no body.
    if not (changelog_section(changelog, version) or changelog_section(changelog, base)):
        raise Refusal("CHANGELOG.md has no '## %s' section with a body; release.yml would fail after the builds" % base)
    if changelog_section(changelog, "Unreleased") is not None:
        raise Refusal("CHANGELOG.md still has '## Unreleased' at this commit: the bump dates it, "
                      "so this is not the release")


def cmd_prep(args):
    new = args[0] if len(args) == 1 else ""
    if not parse(new):
        die("usage: prep <version>, like 0.4.0 or 0.4.0-rc.1 (no leading v, no leading zeros)")
    current = branch()
    check_prep_branch(current)
    clean()
    fetch()
    listing = out("git", "ls-remote", "--tags", "origin", "refs/tags/v*", why="could not list origin's tags")

    cargo_toml = read("Cargo.toml")
    old = crate_version(cargo_toml)
    check_later(new, old, listing)
    # Branch protection wants the branch up to date with main before it merges. Bringing
    # main in after the bump would put a merge commit after it, so main comes in first.
    if not ok("git", "merge-base", "--is-ancestor", "origin/main", "HEAD"):
        die("%s is behind origin/main; merge origin/main into it first, so the bump is the "
            "last commit the branch adds" % current)
    check_tag_absent(new, listing, ok("git", "rev-parse", "--quiet", "--verify", "refs/tags/v" + new))

    # Every file's new text is worked out, and every refusal made, before the first write:
    # a refusal leaves nothing to clean up.
    final = new.split("-")[0] == new
    changes = {"Cargo.toml": re.sub(r'^version = "[^"]*"$', 'version = "%s"' % new,
                                    cargo_toml, count=1, flags=re.M)}
    changes["CHANGELOG.md"] = date_changelog(read("CHANGELOG.md"), new, datetime.date.today().isoformat())
    if final:
        # The install pages name the last release, which after a candidate is not the
        # crate's version. docs/install.md's `version=` line says which one they name.
        shown = shown_version(read("docs/install.md"))
        for page in INSTALL_PAGES:
            changes[page] = name_new_version(page, read(page), shown, new)

    subject = "chore(release): " + new
    if DRY:
        say("would write %s -> %s into %s, and the three workspace crates into Cargo.lock"
            % (old, new, ", ".join(sorted(changes))))
        say("would commit '%s' on %s, the branch checked out now" % (subject, current))
        return
    before = out("git", "rev-parse", "HEAD")
    files = ["Cargo.toml", "Cargo.lock", "CHANGELOG.md"] + (list(INSTALL_PAGES) if final else [])
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
        must("git", "add", *files)
        must("git", "commit", "--quiet", "-m", subject, "-m", body)
    except SystemExit:
        # The tree was clean at the start, so the undo puts back exactly what this wrote.
        if out("git", "rev-parse", "HEAD") != before:
            undo = "git reset --hard HEAD~1"
        else:
            undo = "git restore --staged --worktree -- " + " ".join(files)
        say("stopped halfway, on %s. To undo: %s" % (current, undo))
        raise
    subprocess.run(["git", "--no-pager", "show", "-U0", "--format=%h %s%n", "HEAD", "--",
                    "Cargo.toml", "CHANGELOG.md", *INSTALL_PAGES])
    say("every changed line is above (Cargo.lock: the three workspace crates)")
    say("to take it back before the push: git reset --hard HEAD~1")
    say("next: push %s, open its pull request (or let the open one take the commit), "
        "then: just merge" % current)


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
        release = merged_release(number)
        if release == out("git", "rev-parse", "HEAD"):
            say("next: just release-tag")
        elif release:
            say("next: just release-tag %s (main has moved past the merge that carried the "
                "bump; the tag goes on that merge, named by its hash)" % release[:12])


def merged_release(number):
    """The merge commit this pull request just made, when it is one tag would take; None
    otherwise. The predicate is asked of the merge commit, not of the branch's head: an
    update from main merged into the branch after the bump leaves the bump in the head's
    history but not in the head's own diff."""
    done = subprocess.run(["gh", "pr", "view", number, "--json", "mergeCommit"],
                          stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, universal_newlines=True)
    try:
        oid = json.loads(done.stdout)["mergeCommit"]["oid"]
    except (ValueError, KeyError, TypeError):
        return None
    if not ok("git", "cat-file", "-e", oid + "^{commit}"):
        return None
    try:
        check_release_merge(*release_merge_facts(oid))
    except Refusal:
        return None
    return oid


def release_merge_facts(sha):
    """What check_release_merge asks of a commit: its parent count, the first-parent diff
    of Cargo.toml, and the crate version at the commit."""
    parents = len(out("git", "rev-list", "--parents", "-n", "1", sha).split()) - 1
    diff = out("git", "diff", sha + "^1", sha, "--", "Cargo.toml") if parents else ""
    return parents, diff, crate_version(out("git", "show", sha + ":Cargo.toml"))


def untagged_release_merge(listing):
    """The newest merge on main's first-parent line that carried a bump whose tag origin
    lacks, or None: named in tag's refusal when main's tip is not that merge."""
    released = remote_versions(listing)
    for sha in out("git", "rev-list", "--first-parent", "--merges", "-n", "50", "origin/main").split():
        try:
            facts = release_merge_facts(sha)
            check_release_merge(*facts)
        except Refusal:
            continue
        return None if facts[2] in released else sha
    return None


def ci_runs(sha):
    return gh_json("run", "list", "--commit", sha, "--workflow", "ci.yml",
                   "--json", "databaseId,status,conclusion")


def cmd_tag(args):
    by_hand("tag")
    if len(args) > 1:
        die("usage: tag [commit] (the commit defaults to main's tip; the version comes from its Cargo.toml)")
    if branch() != "main":
        die("tags are made on main")
    clean()
    at_origin_main()
    named = args[0] if args else "HEAD"
    sha = out("git", "rev-parse", "--verify", "--quiet", named + "^{commit}", why="no commit named " + named)
    if not ok("git", "merge-base", "--is-ancestor", sha, "origin/main"):
        die("%s is not on origin/main; the tag goes on a merge main already holds" % named)
    parents, cargo_diff, version = release_merge_facts(sha)
    tag = "v" + version

    if out("git", "ls-remote", "--tags", "origin", "refs/tags/" + tag):
        die("%s is on GitHub already: the crate version was not bumped, or this release is cut" % tag)
    try:
        check_release_merge(parents, cargo_diff, version)
    except Refusal as refusal:
        found = None if args else untagged_release_merge(
            out("git", "ls-remote", "--tags", "origin", "refs/tags/v*", why="could not list origin's tags"))
        if found:
            die("%s is %s. Something merged after the release merge; tag that merge by its "
                "hash: just release-tag %s" % (named, refusal, found[:12]))
        die("%s is %s" % (named, refusal))
    reuse = ok("git", "rev-parse", "--quiet", "--verify", "refs/tags/" + tag)
    if reuse:
        # A local tag that never reached GitHub: an earlier run whose push did not finish.
        if out("git", "rev-parse", tag + "^{commit}") != sha:
            die("a local %s sits on another commit and was never pushed; "
                "remove it (git tag -d %s) and run this again" % (tag, tag))
        say("reusing the local %s: it is on this commit and not on GitHub" % tag)

    check_tag_notes(out("git", "show", sha + ":CHANGELOG.md"), version)

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

    done = subprocess.run(["git", "describe", "--tags", "--abbrev=0", "--match", "v*", "--exclude", tag, sha],
                          stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, universal_newlines=True)
    previous = done.stdout.strip() if done.returncode == 0 else ""
    say("%s on %s; previous tag %s; ci green"
        % (tag, out("git", "log", "-1", "--format=%h %s", sha), previous or "none"))
    confirm("tag and publish %s? A published release is not taken back" % tag)
    if not reuse:
        must("git", "tag", "-a", tag, "-m", tag, sha)
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


# The self-test: the pure helpers above, over made-up text, with no repository and no
# network. `just lint` runs it, so CI runs it on both runners and release.yml runs it again
# on the tag.

CHANGELOG_UNRELEASED = """# Changelog

## Unreleased

### Added

- A new thing.

## 0.6.0 - 2026-09-26

### Fixed

- An old thing.
"""

CHANGELOG_AFTER_RC = """# Changelog

## 0.7.0 - 2026-09-20

### Added

- A new thing.

## 0.6.0 - 2026-09-26

- An old thing.
"""

INSTALL_MD = """# Install

```sh
version=0.6.0
```

cargo install --tag v0.6.0 lastcall

Not these: 10.6.0, 0.6.0-rc.1, 0.6.01, 0.6.0.1.
"""

CARGO_TOML = """[workspace]
members = ["crates/a"]

[workspace.package]
version = "0.6.0"
edition = "2024"

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
"""

LS_REMOTE = """1111111111111111111111111111111111111111\trefs/tags/v0.5.0
2222222222222222222222222222222222222222\trefs/tags/v0.5.0^{}
3333333333333333333333333333333333333333\trefs/tags/v0.6.0
4444444444444444444444444444444444444444\trefs/tags/v0.6.0^{}
5555555555555555555555555555555555555555\trefs/tags/vnext
"""

LS_V070 = """6666666666666666666666666666666666666666\trefs/tags/v0.7.0
7777777777777777777777777777777777777777\trefs/tags/v0.7.0^{}
"""

BUMP_DIFF = """diff --git a/Cargo.toml b/Cargo.toml
index 07e4e7c..91925a7 100644
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -7,7 +7,7 @@ members = [
 ]
 
 [workspace.package]
-version = "0.6.0"
+version = "0.7.0"
 edition = "2024"
 license = "MIT OR Apache-2.0"
"""

DEPS_DIFF = """diff --git a/Cargo.toml b/Cargo.toml
index 07e4e7c..91925a8 100644
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -20,7 +20,7 @@ lastcall-testkit = { path = "crates/lastcall-testkit" }
 
-serde = { version = "1.0.1", features = ["derive"] }
+serde = { version = "1.0.2", features = ["derive"] }
"""


def cmd_self_test(args):
    if args:
        die("usage: self-test")
    failures = []
    ran = [0]

    def check(name, got, want):
        ran[0] += 1
        if got != want:
            failures.append("%s: got %r, want %r" % (name, got, want))

    def refusal(fn, *fn_args):
        """The refusal a helper raises, or None when it lets the input through."""
        try:
            fn(*fn_args)
        except Refusal as refused:
            return str(refused)
        return None

    def refuses(name, fragment, fn, *fn_args):
        said = refusal(fn, *fn_args)
        check(name, said is not None and fragment in said, True)
        if said is not None and fragment not in said:
            failures.append("%s: the refusal read %r" % (name, said))

    def passes(name, fn, *fn_args):
        check(name + " (no refusal)", refusal(fn, *fn_args), None)

    def value(name, fn, *fn_args):
        """What a helper returns; a refusal where none was expected is this case failing."""
        try:
            return fn(*fn_args)
        except Refusal as refused:
            failures.append("%s: refused: %s" % (name, refused))
            return ""

    # Versions: the shape, and the order a release follows.
    for text, valid in (("0.7.0", True), ("0.7.0-rc.1", True), ("10.0.0", True),
                        ("v0.7.0", False), ("0.07.0", False), ("0.7", False),
                        ("0.7.0-rc.0", False), ("0.7.0-beta.1", False), ("", False)):
        check("parse %r" % text, parse(text) is not None, valid)
    for new, old, want in (("0.7.0", "0.6.0", True), ("0.6.0", "0.6.0", False),
                           ("0.5.9", "0.6.0", False), ("0.10.0", "0.9.0", True),
                           ("0.7.0-rc.1", "0.6.0", True), ("0.7.0", "0.7.0-rc.1", True),
                           ("0.7.0-rc.1", "0.7.0", False), ("0.7.0-rc.2", "0.7.0-rc.1", True),
                           ("0.7.0", "garbage", False)):
        check("later(%s, %s)" % (new, old), later(new, old), want)

    # The crate version is the one top-level `version =` line.
    check("crate_version", value("crate_version", crate_version, CARGO_TOML), "0.6.0")
    refuses("crate_version with two top-level lines", "found 2",
            crate_version, CARGO_TOML + 'version = "9.9.9"\n')

    # The CHANGELOG dating.
    dated = value("Unreleased is dated", date_changelog, CHANGELOG_UNRELEASED, "0.7.0", "2026-09-27")
    check("Unreleased is dated", "## 0.7.0 - 2026-09-27\n\n### Added\n\n- A new thing." in dated, True)
    check("Unreleased is gone", "## Unreleased" in dated, False)
    check("the older section is untouched", "## 0.6.0 - 2026-09-26" in dated, True)
    check("a candidate dates its base",
          value("a candidate dates its base", date_changelog, CHANGELOG_UNRELEASED, "0.7.0-rc.1",
                "2026-09-27").count("## 0.7.0 - 2026-09-27"), 1)
    after_rc = value("the final after a candidate", date_changelog, CHANGELOG_AFTER_RC, "0.7.0", "2026-09-27")
    check("the final after a candidate takes today's date",
          ("## 0.7.0 - 2026-09-27" in after_rc, "2026-09-20" in after_rc), (True, False))
    refuses("both the dated section and Unreleased", "has both", date_changelog,
            CHANGELOG_AFTER_RC.replace("# Changelog\n", "# Changelog\n\n## Unreleased\n\n- more\n"),
            "0.7.0", "2026-09-27")
    refuses("an empty Unreleased", "is empty", date_changelog,
            "# Changelog\n\n## Unreleased\n\n## 0.6.0 - 2026-09-26\n\n- old\n", "0.7.0", "2026-09-27")
    refuses("neither section", "neither", date_changelog,
            "# Changelog\n\n## 0.6.0 - 2026-09-26\n\n- old\n", "0.7.0", "2026-09-27")

    # The install pages: the shown version, the substitution and the two-hits rule.
    check("shown_version", value("shown_version", shown_version, INSTALL_MD), "0.6.0")
    refuses("an install page with no version= line", "no 'version=", shown_version, "# Install\n")
    named = value("two hits", name_new_version, "docs/install.md", INSTALL_MD, "0.6.0", "0.7.0")
    check("both hits move", ("version=0.7.0" in named, "--tag v0.7.0" in named), (True, True))
    check("the look-alikes stay", "10.6.0, 0.6.0-rc.1, 0.6.01, 0.6.0.1." in named, True)
    refuses("three hits", "3 times", name_new_version, "README.md",
            INSTALL_MD + "v0.6.0\n", "0.6.0", "0.7.0")
    refuses("one hit", "1 times", name_new_version, "README.md", "v0.6.0\n", "0.6.0", "0.7.0")

    # What tag checks in the release commit's CHANGELOG.md.
    passes("the notes of a dated release", check_tag_notes, CHANGELOG_AFTER_RC, "0.7.0")
    passes("a candidate ships its base's notes", check_tag_notes, CHANGELOG_AFTER_RC, "0.7.0-rc.1")
    refuses("a version with no section", "no '## 0.8.0'", check_tag_notes, CHANGELOG_AFTER_RC, "0.8.0")
    refuses("Unreleased still there", "Unreleased", check_tag_notes,
            dated.replace("# Changelog\n", "# Changelog\n\n## Unreleased\n\n- more\n"), "0.7.0")

    # The branch prep commits on: any but main.
    refuses("prep on main", "main", check_prep_branch, "main")
    for name in ("feat/range-select", "release/v0.7.0", "fix/a-thing"):
        passes("prep on " + name, check_prep_branch, name)

    # The version against the crate and against origin's tags, over a fake ls-remote listing.
    check("remote_versions", sorted(remote_versions(LS_REMOTE)), ["0.5.0", "0.6.0"])
    passes("0.7.0 over 0.6.0", check_later, "0.7.0", "0.6.0", LS_REMOTE)
    passes("0.7.0 with no tags at all", check_later, "0.7.0", "0.6.0", "")
    refuses("not later than the crate", "the crate's 0.6.0", check_later, "0.6.0", "0.6.0", LS_REMOTE)
    refuses("origin already has v0.7.0", "v0.7.0, the newest tag on origin",
            check_later, "0.7.0", "0.6.0", LS_REMOTE + LS_V070)
    refuses("origin has a later tag than the crate", "the newest tag on origin",
            check_later, "0.7.0", "0.6.0", LS_REMOTE + LS_V070.replace("0.7.0", "0.8.0"))
    passes("the final after its candidate", check_later, "0.7.0", "0.7.0-rc.1",
           LS_REMOTE + LS_V070.replace("0.7.0", "0.7.0-rc.1"))
    refuses("a candidate already tagged", "the newest tag on origin", check_later, "0.7.0-rc.1", "0.6.0",
            LS_REMOTE + LS_V070.replace("0.7.0", "0.7.0-rc.1"))
    passes("the tag is absent", check_tag_absent, "0.7.0", LS_REMOTE, False)
    refuses("the tag is on origin", "on origin", check_tag_absent, "0.7.0", LS_REMOTE + LS_V070, False)
    refuses("the tag is local", "locally", check_tag_absent, "0.7.0", LS_REMOTE, True)

    # The commit tag takes: a merge whose first-parent diff carries the bump.
    passes("the release merge", check_release_merge, 2, BUMP_DIFF, "0.7.0")
    refuses("a plain commit", "not a merge commit", check_release_merge, 1, BUMP_DIFF, "0.7.0")
    refuses("an octopus", "not a merge commit", check_release_merge, 3, BUMP_DIFF, "0.7.0")
    refuses("a merge without the version line", "version line", check_release_merge, 2, DEPS_DIFF, "0.7.0")
    refuses("a merge that leaves Cargo.toml alone", "version line", check_release_merge, 2, "", "0.7.0")
    refuses("a merge that bumps rust-version only", "version line", check_release_merge, 2,
            BUMP_DIFF.replace('-version = "0.6.0"\n+version = "0.7.0"',
                              '-rust-version = "1.98.0"\n+rust-version = "1.99.0"'), "0.7.0")
    refuses("a merge that bumps to another version", "version line", check_release_merge, 2, BUMP_DIFF, "0.8.0")

    for failure in failures:
        say("self-test FAILED " + failure)
    if failures:
        die("self-test: red, %d line(s) above over %d checks" % (len(failures), ran[0]))
    say("self-test: %d checks passed" % ran[0])


def main():
    verbs = {"prep": cmd_prep, "merge": cmd_merge, "tag": cmd_tag}
    if len(sys.argv) >= 2 and sys.argv[1] == "self-test":
        # No repository and no network: it runs wherever the script is.
        cmd_self_test(sys.argv[2:])
        return
    if len(sys.argv) < 2 or sys.argv[1] not in verbs:
        die("usage: release.py prep <version> | merge [number] | tag [commit] | self-test")
    top = subprocess.run(["git", "rev-parse", "--show-toplevel"], stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL, universal_newlines=True)
    if top.returncode != 0:
        die("not inside the repository")
    os.chdir(top.stdout.strip())
    try:
        # `just` passes an optional argument left out as an empty string.
        verbs[sys.argv[1]]([a for a in sys.argv[2:] if a])
    except Refusal as refusal:
        die(str(refusal))
    except KeyboardInterrupt:
        die("interrupted")


if __name__ == "__main__":
    main()
