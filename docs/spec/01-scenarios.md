# lastcall scenarios: the executable spec

Companion to [`00-spec.md`](00-spec.md) §6 and §7.4. Every scenario is a **setup** (a git/shell script), an **action** (what an agent or the user does), and the **expected pile** — the exact pending rows, annotations, group rows, and notices lastcall must show — plus what must survive a **restart** (kill and relaunch, ledger reloaded, everything recomputed per invariant 1). "Pile" means the left-nav file rows for that repo.

Conventions: the repo root is `R`; `origin` is a local bare repo; the "agent" is any process; `seen` means the current seen tree; `over-show` means content shows as pending even though the user might consider it reviewed elsewhere — allowed; **hidden** means content differs from the seen state and does not show — a bug, always.

Legend for the harness column: **H** = verifiable with the git-plumbing harness (§7.4) before any product code exists; **E** = needs the engine (Phase 2 integration test); **U** = needs the TUI (Phase 3+); **S** = sponsor-demonstrated.

---

## A. Core seen-state loop

**A1 — First sight of a git repo.** Setup: repo with 3 committed files, `f1` has an uncommitted edit. Action: launch. Expect: `seen = HEAD^{tree}`; pile = `[f1]`; `seen_at.head_commit = HEAD`. Restart: identical. *(H)*

**A2 — Edit, review, accept file.** Setup: A1. Action: user accepts `f1`. Expect: override `f1 → blob(rendered f1)`; pile = `[]`. Restart: `[]`. Agent edits `f1` again → pile `[f1]` showing only the *new* delta (baseline is the override). *(H)*

**A3 — Accept hunk.** Setup: `f1` has two separated hunks. Action: accept hunk 1. Expect: override blob = seen blob ⊕ hunk 1; pile `[f1]` with exactly hunk 2 remaining. Property: recomputing never resurrects hunk 1 and never drops hunk 2, across random further edits to other regions. *(E, proptest)*

**A4 — Accept all folds to a new tree.** Setup: 5 files pending, 2 with hunk-level overrides. Action: accept all. Expect: `seen` = write-tree of rendered content; overrides cleared (flags retained); pile `[]`; `git --git-dir=objects ls-tree seen` lists the exact rendered blobs. Restart: `[]`. *(H)*

**A5 — Accept-all is CAS per file.** Setup: 3 files pending. Action: user opens confirm; agent rewrites `f2` before the user confirms; user confirms. Expect: new tree contains the *rendered* `f2` (what the user saw), not the live one; next scan shows `f2` pending with only the post-confirm delta. Nothing hidden. *(H: simulate by writing the rendered blob to index-info while disk differs)*

**A6 — Accept file is CAS.** Setup: `f1` pending. Action: agent writes `f1` between render and click. Expect: accept refused, re-render, `f1` still pending with the newer content. *(E)*

**A7 — Accept deletion / restore deletion.** Setup: agent deletes tracked `f3`. Expect: pile `[f3 (deleted)]`. Action: accept deletion → override `f3: null`; pile `[]`. Agent recreates `f3` with the old content → pile `[f3]` (baseline is "absent"). Alternatively restore the deletion → `f3` recreated byte-identical to baseline blob; restore asserts absence with a case-sensitive `readdir`. *(H for ledger math; E for restore)*

**A8 — Flag with note, export.** Setup: `f1` pending. Action: flag hunk 2 with note "why is this unwrap safe?". Expect: override has `flag`, baseline unchanged, pile `[f1 ⚑]`; export = path + hunk + note (+ attribution if known). Accept-all keeps the flag as flag-only. *(E)*

## B. History operations (the bugs the old model had)

**B1 — Edit then commit (the §1.1 case).** Setup: A1 (`f1` pending). Action: agent `git commit -am "x"`. Expect: pile still `[f1]` with the same hunks; notice "committed on main (1 commit)". HEAD moved, baseline did not. Restart: `[f1]`. *(H)*

**B2 — Edit then `checkout -b`.** Action: agent `git checkout -b feat-x` with `f1` uncommitted. Expect: pile `[f1]` unchanged; branch label `feat-x`; notice "switched main → feat-x (same commit)". *(H)*

**B3 — Branch label flip-flop.** Action: `checkout main`, `checkout feat-x` at the same commit. Expect: pile unchanged both times; only the label moves. *(H)*

**B4 — Switch to a divergent branch and back.** Setup: `feat-y` differs from `main` in `g1`,`g2`; user has an override on `f1`. Action: `checkout feat-y`. Expect: pile `[g1, g2]` (over-show; notice "switched main → feat-y: 2 files differ from seen state"); override on `f1` untouched. Action: `checkout main`. Expect: pile `[]` (content matches seen again); `f1` override intact. *(H)*

**B5 — Rebase preserving content.** Setup: `feat-x` has 2 local commits reviewed and accepted (accept-all done at its tip). Action: agent `git rebase main` (no conflicts, content of the 2 commits unchanged relative to their bases; upstream `main` touched `u1`). Expect: pile = `[upstream · 1 file]` group only (`u1`); the rebased commits re-flag nothing; notice "rebased feat-x onto main". *(H)*

**B6 — Interactive rebase / amend that changes content.** Action: agent amends the last commit, changing `f2`'s content. Expect: pile `[f2]` showing exactly the amended delta; nothing else. *(H)*

**B7 — `reset --hard HEAD~1` on accepted work.** Setup: accept-all at tip `T`. Action: `git reset --hard T~1`. Expect: pile shows the files T changed, as *pending in reverse* (content now differs from seen); notice "reset: moving to T~1". Over-show, correct. *(H)*

**B8 — Stash / pop.** Setup: `f1` pending. Action: `git stash`. Expect: pile `[]` (content not on disk is not pending), notice "stashed 1 file". Action: `git stash pop`. Expect: pile `[f1]` with the identical hunks. *(H)*

**B9 — Agent pushes.** Setup: B1 (committed, pending). Action: `git push -u origin main`. Expect: pile unchanged `[f1]`. (The old model's sticky-classification hole does not exist: baseline is content.) *(H)*

## C. Upstream: the flood shaper

**C1 — Fetch only.** Action: `git fetch` (origin/main advanced by 3 commits touching 40 files). Expect: pile unchanged; no notice. *(H)*

**C2 — Fast-forward pull.** Setup: user's `main` behind origin by 40 files, no local work. Action: `git pull --ff-only`. Expect: pile = `[upstream · 40 files]` one group row; expanding it lists the 40; accept-group → `[]`. Individual rows: none. *(H)*

**C3 — Merge origin/main into a feature branch with uncommitted work (the sponsor's scenario).** Setup: on `feat-x` with anchor at its reviewed tip; `parse.rs`, `lexer.rs` uncommitted and pending; origin/main has 40 new files, none overlapping. Action: agent `git merge origin/main` (clean). Expect: pile = `[parse.rs, lexer.rs, upstream · 40 files]`; the merge commit contributes nothing (`diff-tree --cc` empty). *(H)*

**C4 — Merge with conflicts, resolved by the agent.** Setup: C3 but 3 upstream files conflict with feat-x commits. Action: `git merge origin/main` → conflicts; agent resolves and commits `G`. Expect during conflict: pile shows the 3 conflicted files as individual rows with the "includes upstream changes" badge (content has markers; `MERGE_HEAD` is a classification head), restore disabled on them, the other 37 already grouped as `upstream`, no transition notice yet. After `G`: pile = `[parse.rs, lexer.rs, c1 ⓤ, c2 ⓤ, c3 ⓤ, upstream · 37 files]` — the 3 resolutions are individual rows with the badge (paths in `diff-tree --cc G`), the other 37 grouped. *(H — verified)*

**C5 — Both sides touched the same file.** Setup: local commit changed `m.rs` lines 1–10; upstream changed `m.rs` lines 200–210. Action: merge. Expect: `m.rs` is an individual row with badge "includes upstream changes"; the hunk at 200–210 carries the same label; the hunk at 1–10 does not. Nothing grouped away. *(H for file-level; U for hunk-level)*

**C6 — Upstream file with an uncommitted delta on top.** Setup: C2 but the agent then edits `u7` (one of the 40). Expect: `[upstream · 39 files, u7]` — `u7` individual with badge "includes upstream changes". *(H)*

**C7 — Annotation range fallback.** Setup: `seen_at.head_commit` no longer an ancestor of HEAD (rebase). Expect: range = `merge-base..HEAD`; annotation still computed; if no merge-base (unrelated history), no annotation, all individual rows. *(H)*

**C8 — Pushed-from-elsewhere agent branch (known heuristic limit).** Setup: an agent on another machine pushed `feat-z`; user `checkout feat-z`. Expect: over-show of the branch delta with rows grouped as `upstream` (they are remote-reachable). Deferral-ledger entry; one un-group click. Never hidden. *(H)*

## D. Content model edge cases

**D1 — Mode-only change.** Action: `chmod +x run.sh`. Expect: pile `[run.sh]` with one hunk "mode 100644 → 100755"; accept file records mode; `core.fileMode=false` repos ignore it. *(H)*

**D2 — Symlink.** Setup: `link → ../outside/target`. Action: agent repoints the link. Expect: pile `[link]` with hunk showing link-text change; restore = unlink + symlink; the file at `../outside/target` is never opened for writing (assert its mtime/bytes unchanged). *(H for model; E for restore)*

**D3 — CRLF with `text=auto`.** Setup: file with CRLF line endings in a repo with `.gitattributes` `* text=auto`. Action: agent changes one line. Expect: one hunk (numstat 1/1, matching `git diff`), not whole-file churn. Requires hashing with cwd at the root and a relative path; `--stdin --path` from outside the work tree yields 3/3. *(H — verified)*

**D4 — Case-only rename on macOS.** Action: `mv f.txt F.txt`. Expect: git root: pile `[]` or a paired rename row (git sees no change on a case-insensitive FS); draft root: delete+add pair; restore-deletion refuses because case-sensitive listing finds `F.txt`. *(H on macOS only)*

**D5 — Unstaged rename.** Action: agent `mv d/old.rs d/new.rs` and edits 2 lines. Expect: one paired rename row "old.rs → new.rs" with 2-line hunk (similarity pairing via temp index), ledger stores delete + add. *(H)*

**D6 — Sparse checkout.** Setup: `git sparse-checkout set src/`. Expect: files outside `src/` do not appear as pending deletions — they carry the skip-worktree bit in the **user's** index, which the scan must consult (our private index knows nothing of it). *(H — verified)*

**D7 — Lockfile churn (collapsed class).** Action: `npm install` rewrites `package-lock.json` (4,000 lines). Expect: one collapsed row `package-lock.json (collapsed, +2,113/−1,887)` with single accept; no hunk view by default. *(E/U)*

**D8 — Binary and oversize.** Action: agent adds a 2 MB PNG and a 600 KiB generated file. Expect: both collapsed rows (binary; > `collapse_size_bytes`). *(E)*

**D9 — Nested repo.** Setup: `R/vendor/lib` is its own git repo. Action: agent edits `R/vendor/lib/x.c`. Expect: shows under root `R/vendor/lib`, not `R`; if `vendor/**` is in `ignore_globs` the watcher is slower but the next scan of that root still shows it. *(E)*

**D10 — Linked worktree.** Setup: `git worktree add ../R-wt feat-w`. Expect: `R-wt` is its own root with its own ledger/store (alternates → common dir); its HEAD watcher points at `.git/worktrees/R-wt/HEAD`; badge "worktree of R". Agent commits in `R-wt` → B1 behavior there only. *(H)*

**D11 — Paths with unicode and spaces.** Setup: `docs/résumé draft.md`. Expect: appears, accepts, restores correctly (all plumbing uses `-z`). *(H)*

## E. Storage and crash safety

**E1 — Crash mid-accept.** Action: kill -9 between object write and ledger rename. Expect on restart: ledger is the previous version; orphan object harmless; pile identical to pre-accept. *(E)*

**E2 — Corrupt override.** Action: hand-edit an override to reference a missing blob. Expect: that path falls to the seen-tree baseline (over-show), one-line notice; nothing else affected. *(H)*

**E3 — Alternates read-miss.** Action: delete an object from the user's repo that our store referenced via alternates. Expect: that path re-flags in full (fail open); notice. *(H)*

**E4 — Root moved.** Action: `mv ~/dev/x ~/dev/y` and relaunch. Expect: treated as a new root (nothing seen → first-sight rules); notice pointing at the old state dir. *(E)*

**E5 — Compaction.** Setup: 600 overrides. Expect: automatic fold into a new tree; pile unchanged before/after; overrides ≤ threshold. *(H)*

## F. Draft roots

**F1 — Draft dir first sight (default `seen`).** Setup: `_drafts/` with 200 files, gitignored. Expect: pile `[]`; seen tree = write-tree of current content. Action: agent edits `_drafts/reply.md` → pile `[_drafts/reply.md]` with hunks; accept → `[]`. *(H)*

**F2 — Draft dir with `draft_initial = pending`.** Expect: all 200 pending; accept-all → `[]`. *(H)*

**F3 — Non-git draft dir.** Setup: `~/notes` (no `.git` anywhere above). Expect: same behavior as F1 using our private store as the repository and `~/notes` as the work tree. *(H)*

## G. herdr integration (mock server unless marked S)

**G1 — Bootstrap.** Expect: subscribe → ack → snapshot → resync-if-buffered; no event applied as state. *(E)*

**G2 — Both envelope shapes.** Expect: `pane_created` (snake) and `pane.agent_status_changed` (dotted, untagged data) parsed on one connection. *(E)*

**G3 — Done flip without a global event.** Setup: pane goes `done`; user focuses the tab in herdr (no lifecycle event carries the flip). Expect: per-pane subscription's poll fallback and/or the `tab_focused` resync clears our indicator within the fallback interval; never a stale flag past 30 s. *(E against pinned herdr)*

**G4 — Subscribe on a dead pane.** Expect: `pane_not_found` at construction → "pane gone", no retry loop. *(E)*

**G5 — Orphan status connection.** Setup: `pane_closed` suppressed. Expect: next resync tears the connection down. *(E)*

**G6 — Disconnect / reconnect.** Expect: standalone badge, reconnect, re-bootstrap, state converges. *(E)*

**G7 — Toast.** Setup: `toast.delivery = herdr`. Expect: our call at the `done` instant returns `busy`; delayed retry shows; with `delivery = off` we get `disabled` and never retry. *(E)*

**G8 — Live demo.** Agent finishes in the left pane; repo flags in the right pane. *(S)*

## H. Attribution (post-v1)

**H1 — Hook enrichment.** Setup: Claude Code `PostToolUse` adapter installed. Action: agent edits `f1` via `Edit`. Expect: pile row `f1 · Claude Code · session abc`; flag export includes it; removing the adapter changes nothing about pending. *(E, later phase)*

**H2 — Bash-mediated edit.** Action: agent runs `cargo fmt` via Bash. Expect: files show as pending via the watcher with no attribution (or "Bash · session abc" if the adapter forwards the Bash hook); never missing. *(E, later phase)*

---

## Harness notes (§7.4)

**Status 2026-09-01:** `scripts/harness/scenarios.sh` (run: `bash scripts/harness/scenarios.sh`; fixtures go to `scripts/harness/work/`, gitignored, or `$LC_WORK`) — 46 assertions, all passing. Scenarios marked *verified* above ran green; the remaining H scenarios (A3 property, A6, D2 restore, D5 pairing, E1/E3/E4) are Phase 2 tests.

The pre-Phase-2 harness implements only the plumbing the engine will call — no TUI, no watcher, no ledger JSON — as shell functions: `lc_init_store R` (bare store + alternates), `lc_first_sight`, `lc_scan` (private index refresh + `diff-files` + `ls-files --others`), `lc_accept_file`, `lc_accept_all` (index-info + write-tree), `lc_upstream_paths` (rev-list `--not --remotes`, `diff-tree --cc` for merges), and `lc_pile` (composes the expected output). Every **H** scenario above is one function call sequence with an `assert_pile` at each Expect line. A failing assertion is a spec bug to fix here first, not a test to loosen.
