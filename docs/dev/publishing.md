# Publishing: the day the repository goes public

Maintainer only. Everything here is done once, in this order, on the day `aarontimko/lastcall`
flips from private to public. Each line is one action with the `gh` command that does it, or
the Settings path when there is no command. Run them from a shell authenticated as the
repository owner (`gh auth status`).

**Done 2026-09-12.** The flip, steps 2 to 5 and the two tags (`v0.1.0-rc.1`, `v0.1.0`) all
happened that day; the record is the 2026-09-12 entry in `docs/spec/00-spec.md` §10. Two
things differed from the text below. The maintainer's install transcript came from his own
Mac in a fresh directory, with the container smoke standing in for the machine with nothing
on it. And a `curl` download carries no quarantine attribute, which `docs/install.md` now
says. What to do after the flip lives in [`operations.md`](operations.md).

## 1. Before the flip

- Working tree clean, `main` up to date, `just lint` and `just test-prepush` green on `main`.
- The disclosure grep below has zero hits. Do this first: it is the only irreversible one.
- The README says build from source: the first tag comes after the flip (attestation
  needs a public repository), so there is no release to point at on the day.
- `gh --version` is 2.60 or newer (`--accept-visibility-change-consequences` below needs it).
- `LICENSE-MIT`, `LICENSE-APACHE`, `NOTICE`, `CONTRIBUTING.md`, `SECURITY.md`,
  `CODE_OF_CONDUCT.md` all present at the root.
- The README's GIF is `docs/demo/lastcall.gif`, recorded with VHS from `docs/demo/`:
  `stage.sh` builds the three demo repositories and the tape, `vhs` records, `render.sh`
  encodes. Re-record after a change to the list, the diff pane, the flag modal or the
  help overlay; the header comments in `stage.sh` say what to watch for.

## 2. Repository settings

- Visibility: `gh repo edit aarontimko/lastcall --visibility public --accept-visibility-change-consequences`
- Description and homepage: `gh repo edit aarontimko/lastcall --description "The last call before code ships: an agent-agnostic review ledger for the terminal" --homepage "https://github.com/aarontimko/lastcall"`
- Topics: `gh repo edit aarontimko/lastcall --add-topic rust --add-topic tui --add-topic ratatui --add-topic code-review --add-topic git --add-topic ai-agents --add-topic developer-tools`
- Delete branches on merge: `gh repo edit aarontimko/lastcall --delete-branch-on-merge`
- Merge strategy: merge commits, per ruling P16 in
  [`docs/spec/99-phase9b-release-kickoff.md`](../spec/99-phase9b-release-kickoff.md) (the
  decision log cites branch commits by SHA):
  `gh repo edit aarontimko/lastcall --enable-merge-commit --enable-squash-merge=false --enable-rebase-merge=false`.
  If that ruling ever flips to squash-only, flip `required_linear_history` in step 3 with it.
- Discussions on: `gh repo edit aarontimko/lastcall --enable-discussions`
- Discussion categories: Settings > Discussions, or the Discussions tab's pencil icon. Keep
  **Q&A** and **Ideas**; the issue chooser links here for both.
- Wiki and Projects off unless they are being used: `gh repo edit aarontimko/lastcall --enable-wiki=false --enable-projects=false`
- Private vulnerability reporting on: `gh api -X PUT repos/aarontimko/lastcall/private-vulnerability-reporting` (SECURITY.md sends reporters straight at it)
- Secret scanning and push protection on: `gh api -X PATCH repos/aarontimko/lastcall -f 'security_and_analysis[secret_scanning][status]=enabled' -f 'security_and_analysis[secret_scanning_push_protection][status]=enabled'`
- Dependabot alerts on: `gh api -X PUT repos/aarontimko/lastcall/vulnerability-alerts` (the
  weekly version-bump PRs already come from `.github/dependabot.yml`)

## 3. Branch protection on `main`

A pull request, the CI checks, no force push, no deletion. Take the check
names from a real run first (`gh run view --json jobs --jq '.jobs[].name'`) so a rename in
`ci.yml` does not leave a required check that never reports.

```sh
gh api -X PUT repos/aarontimko/lastcall/branches/main/protection --input - <<'JSON'
{
  "required_status_checks": {
    "strict": true,
    "contexts": [
      "lint (ubuntu-latest)",
      "lint (macos-latest)",
      "unit (ubuntu-latest)",
      "unit (macos-latest)",
      "integration (ubuntu-latest)"
    ]
  },
  "enforce_admins": true,
  "required_pull_request_reviews": {
    "required_approving_review_count": 0,
    "dismiss_stale_reviews": true,
    "require_code_owner_reviews": false
  },
  "restrictions": null,
  "required_linear_history": false,
  "allow_force_pushes": false,
  "allow_deletions": false,
  "required_conversation_resolution": true
}
JSON
```

`required_approving_review_count` is 0 and `require_code_owner_reviews` is false on
purpose while there is one maintainer: GitHub does not let anyone approve their own pull
request, and the sole code owner authors every PR, so either setting would make every merge
unsatisfiable. `CODEOWNERS` still auto-requests the review, which is the useful part. Raise
the count to 1 and turn code-owner reviews on the day a second maintainer exists.
`enforce_admins` is true so the rules bind the owner too: without it, everything except
"no force push" and "no deletion" is advisory for an admin. When a runner is down, re-run
the check (`gh workflow run ci.yml --ref <branch>`) rather than bypassing it.
`required_linear_history` is false because `main` takes merge commits (step 2).

## 4. Labels

```sh
gh label create bug                --color d73a4a --description "Something is broken" --force
gh label create enhancement        --color a2eeef --description "A behaviour lastcall does not have yet" --force
gh label create documentation      --color 0075ca --description "README, AGENTS.md or docs/" --force
gh label create "good first issue" --color 7057ff --description "Small, self-contained, well understood" --force
gh label create "help wanted"      --color 008672 --description "The maintainer would welcome a PR here" --force
gh label create needs-triage       --color ededed --description "Not yet labelled or reproduced" --force
gh label create security           --color b60205 --description "Handled privately; never open one of these in public" --force
gh label create wontfix            --color ffffff --description "Closed with a reason, on purpose" --force
```

The two issue forms apply `needs-triage` plus `bug` or `enhancement`. A label that does not
exist is silently dropped from the issue (the issue still files), so create those three
before the forms are used. `documentation`, `bug`, `enhancement`, `good first issue`,
`help wanted` and `wontfix` already exist as GitHub defaults; `--force` updates their
descriptions. Delete the defaults that will not be used
(`gh label delete duplicate invalid question accessibility --yes`, one per call), and leave
`herdr-compat` alone: the compat workflow creates it on first drift.

## 5. Workflow edits

Both were narrowed while the repository was private and minutes were counted. Put the
pull-request trigger back and open a PR with the change:

- `.github/workflows/ci.yml`: add `pull_request:` under `on:` beside `workflow_dispatch:`
  and `push: branches: [main]`, and delete the comment that explains why it was removed.
- `.github/workflows/scans.yml`: add `pull_request:` under `on:`, and drop the
  `if: ${{ !github.event.repository.private }}` guard on the `codeql` job now that code
  scanning is available. Leave CodeQL "default setup" off in Settings > Code security:
  it conflicts with the workflow's advanced setup. Add a dependency-review job
  (`actions/dependency-review-action`) to the same workflow; it only runs on
  `pull_request` and only on a public repository, which is why it is not there yet.
- Then confirm the required checks from step 3 actually report on that PR.

## 6. Before every push, once public

The repository was private for its whole construction. The history carries whatever was
written then, and a public push cannot be taken back.

```sh
# Zero hits required in the tree. A history hit is either rewritten before the flip or
# accepted on the record (see below).
terms=$(mktemp)
cat ~/.config/oss-publish/denylist.txt z_ignore/oss-denylist.txt 2>/dev/null \
  | grep -v '^[[:space:]]*#' | grep -v '^[[:space:]]*$' > "$terms"
git grep -n -i -F -f "$terms" -- . ':!z_ignore'
git log --all -p | grep -n -i -F -f "$terms"
git log --all -S<term> --oneline    # which commits a history hit came from
```

- The denylist of terms (private hostnames, internal project names, machine names, paths,
  handles, anything that identifies a person) lives **outside every repository**, in the
  maintainer's shared `~/.config/oss-publish/denylist.txt`, one plain term per line with `#`
  comment lines for the reasons; a repo-specific `z_ignore/oss-denylist.txt` is read too if
  it exists. Neither is committed here, because committing the list publishes the list.
- Project names count. A private repository's name quoted as an example in a design note,
  a fixture path or a source comment (a ledger row named after the repository it tracked)
  belongs on the list: it tells a reader the project exists and what it is called.
- Run both greps. The first catches the working tree, the second catches a string that was
  added and later removed. Prove the instrument on a term you know is present before
  trusting a zero.
- `z_ignore/` is gitignored and untracked, so `git grep` would skip it anyway; the
  pathspec makes that explicit and survives someone force-adding a file there. The
  `git log -S` pass is deliberately unfiltered: a term that ever lived in a committed file
  is in the history whatever directory it sat in.
- A hit in the history is not fixed by a commit. Either rewrite (`git filter-repo`) before
  the repository is public, or decide the term is harmless and record that decision.
- The two commands read content, not commit headers. Check the author and committer
  names separately: `git log --all --format='%an <%ae>%n%cn <%ce>' | sort -u`. Every
  commit here carries the maintainer's name beside the noreply address; GitHub renders
  that name on every commit page. Accept it (an attributed author is the norm) or rewrite
  with `git filter-repo --mailmap` before the flip, and record which.
- "No email address" means no real one: placeholder domains (`example.com`, `.invalid`)
  in tests and docs are fine, so grep for real domains, not for a bare `@`.
