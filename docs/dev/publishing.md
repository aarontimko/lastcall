# Publishing: the day the repository goes public

Maintainer only. Everything here is done once, in this order, on the day `aarontimko/lastcall`
flips from private to public. Each line is one action with the `gh` command that does it, or
the Settings path when there is no command. Run them from a shell authenticated as the
repository owner (`gh auth status`).

## 1. Before the flip

- Working tree clean, `main` up to date, `just lint` and `just test-prepush` green on `main`.
- The disclosure grep below has zero hits. Do this first: it is the only irreversible one.
- `gh release view v0.1.0` (or the tag of the day) exists, or accept that the README says
  build from source.
- `LICENSE-MIT`, `LICENSE-APACHE`, `NOTICE`, `CONTRIBUTING.md`, `SECURITY.md`,
  `CODE_OF_CONDUCT.md` all present at the root.

## 2. Repository settings

- Visibility: `gh repo edit aarontimko/lastcall --visibility public --accept-visibility-change-consequences`
- Description and homepage: `gh repo edit aarontimko/lastcall --description "The last call before code ships: an agent-agnostic review ledger for the terminal" --homepage "https://github.com/aarontimko/lastcall"`
- Topics: `gh repo edit aarontimko/lastcall --add-topic rust --add-topic tui --add-topic ratatui --add-topic code-review --add-topic git --add-topic ai-agents --add-topic developer-tools`
- Delete branches on merge: `gh repo edit aarontimko/lastcall --delete-branch-on-merge`
- Merge strategy: see the decision log. (Squash-only versus merge commits is not settled
  here; set it with `gh repo edit --enable-squash-merge --enable-merge-commit=false` or in
  Settings > General > Pull Requests once it is.)
- Discussions on: `gh repo edit aarontimko/lastcall --enable-discussions`
- Discussion categories: Settings > Discussions, or the Discussions tab's pencil icon. Keep
  **Q&A** and **Ideas**; the issue chooser links here for both.
- Wiki and Projects off unless they are being used: `gh repo edit aarontimko/lastcall --enable-wiki=false --enable-projects=false`
- Private vulnerability reporting on: `gh api -X PUT repos/aarontimko/lastcall/private-vulnerability-reporting` (SECURITY.md sends reporters straight at it)
- Secret scanning and push protection on: `gh api -X PATCH repos/aarontimko/lastcall -f 'security_and_analysis[secret_scanning][status]=enabled' -f 'security_and_analysis[secret_scanning_push_protection][status]=enabled'`
- Dependabot alerts on: `gh api -X PUT repos/aarontimko/lastcall/vulnerability-alerts` (the
  weekly version-bump PRs already come from `.github/dependabot.yml`)

## 3. Branch protection on `main`

A pull request, the CI checks, linear history, no force push, no deletion. Take the check
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
  "enforce_admins": false,
  "required_pull_request_reviews": {
    "required_approving_review_count": 0,
    "dismiss_stale_reviews": true,
    "require_code_owner_reviews": true
  },
  "restrictions": null,
  "required_linear_history": true,
  "allow_force_pushes": false,
  "allow_deletions": false,
  "required_conversation_resolution": true
}
JSON
```

`required_approving_review_count` is 0 on purpose while there is one maintainer: GitHub
does not let anyone approve their own pull request, so any higher number would block every
merge. Raise it to 1 the day a second maintainer exists. `enforce_admins` is false for the
same reason, so the owner can merge a hotfix when a runner is down; everything else still
applies.

## 4. Labels

```sh
gh label create bug                --color d73a4a --description "Something is broken" --force
gh label create enhancement        --color a2eeef --description "A behaviour lastcall does not have yet" --force
gh label create docs               --color 0075ca --description "README, AGENTS.md or docs/" --force
gh label create "good first issue" --color 7057ff --description "Small, self-contained, well understood" --force
gh label create "help wanted"      --color 008672 --description "The maintainer would welcome a PR here" --force
gh label create needs-triage       --color ededed --description "Not yet labelled or reproduced" --force
gh label create security           --color b60205 --description "Handled privately; never open one of these in public" --force
gh label create wontfix            --color ffffff --description "Closed with a reason, on purpose" --force
```

The two issue forms already apply `needs-triage` plus `bug` or `enhancement`, so those
three must exist before the repository is public or the forms fail to file.

## 5. Workflow edits

Both were narrowed while the repository was private and minutes were counted. Put the
pull-request trigger back and open a PR with the change:

- `.github/workflows/ci.yml`: add `pull_request:` under `on:` beside `workflow_dispatch:`
  and `push: branches: [main]`, and delete the comment that explains why it was removed.
- `.github/workflows/scans.yml`: add `pull_request:` under `on:`, and drop the
  `if: ${{ !github.event.repository.private }}` guard on the `codeql` job now that code
  scanning is available.
- Then confirm the required checks from step 3 actually report on that PR.

## 6. Before every push, once public

The repository was private for its whole construction. The history carries whatever was
written then, and a public push cannot be taken back.

```sh
# Zero hits required, for every term, in both commands.
git grep -I -e <term> -- . ':!z_ignore'
git log --all -S<term> --oneline
```

- The denylist of terms (private hostnames, internal project names, machine names, paths,
  handles, anything that identifies a person) lives **outside the repository**, in the
  maintainer's own notes. It is not committed here, because committing the list publishes
  the list.
- Run both commands for every term. The first catches the working tree, the second catches
  a string that was added and later removed.
- `z_ignore/` is gitignored and untracked, so `git grep` would skip it anyway; the
  pathspec makes that explicit and survives someone force-adding a file there. The
  `git log -S` pass is deliberately unfiltered: a term that ever lived in a committed file
  is in the history whatever directory it sat in.
- A hit in the history is not fixed by a commit. Either rewrite (`git filter-repo`) before
  the repository is public, or decide the term is harmless and record that decision.
