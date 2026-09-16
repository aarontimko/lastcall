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

**B2 — Edit then `checkout -b`.** Action: agent `git checkout -b feat-x` with `f1` uncommitted. Expect: pile `[f1]` unchanged; branch label `feat-x`; notice "switched main → feat-x (same commit)"; from Amendment v1.12 `feat-x`'s record is a copy of `main`'s at the same commit, the override carried. *(H)*

**B3 — Branch label flip-flop.** Action: `checkout main`, `checkout feat-x` at the same commit. Expect: pile unchanged both times; only the label moves. *(H)*

**B4 — Switch to a divergent branch and back.** Setup: `feat-y` differs from `main` in `g1`,`g2`; user has an override on `f1`. Action: `checkout feat-y`. Expect: pile `[g1, g2]` (over-show; notice "switched main → feat-y: 2 files differ from seen state", from Amendment v1.12 "switched main → feat-y: first time here, seen state carried from main; 2 files pending" because `feat-y` was never checked out while lastcall watched and its tip is not in `main`'s history, so its record is a plain copy of `main`'s); override on `f1` untouched. Action: `checkout main`. Expect: pile `[]` (content matches seen again); `f1` override intact. *(H)*

**B5 — Rebase preserving content.** Setup: `feat-x` has 2 local commits reviewed and accepted (accept-all done at its tip). Action: agent `git rebase main` (no conflicts, content of the 2 commits unchanged relative to their bases; upstream `main` touched `u1`). Expect: pile = `[upstream · 1 file]` group only (`u1`); the rebased commits re-flag nothing; notice "rebased feat-x onto main". *(H)*

**B6 — Interactive rebase / amend that changes content.** Action: agent amends the last commit, changing `f2`'s content. Expect: pile `[f2]` showing exactly the amended delta; nothing else. *(H)*

**B7 — `reset --hard HEAD~1` on accepted work.** Setup: accept-all at tip `T`. Action: `git reset --hard T~1`. Expect: pile shows the files T changed, as *pending in reverse* (content now differs from seen); notice "reset: moving to T~1". Over-show, correct. *(H)*

**B8 — Stash / pop.** Setup: `f1` pending. Action: `git stash`. Expect: pile `[]` (content not on disk is not pending), notice "stashed 1 file". Action: `git stash pop`. Expect: pile `[f1]` with the identical hunks. *(H)*

**B9 — Agent pushes.** Setup: B1 (committed, pending). Action: `git push -u origin main`. Expect: pile unchanged `[f1]`. (The old model's sticky-classification hole does not exist: baseline is content.) *(H)*

**B12 — A writer that never pauses (the debounce cap).** Setup: A1 repo; the watcher at production timings (750 ms trailing edge, 3 s cap from the first event of a burst — Amendment v1.6). Action: a process appends to a file inside the root every 300 ms for 6 s (an app at debug logging into the repo). Expect: the root is scanned about 3 s after the first event and about every 3 s while the writes continue, then once more 750 ms after the last write; every scan shows the file's content at that moment; no other file in the root is starved; a `HEAD` change mid-burst scans at once and opens a fresh 3 s window; a quiet root is never moved by another root's cap. Without the cap: one scan 750 ms after the burst ends and none during it. *(E — `watcher_debounce_cap_scans_a_never_quiet_root_about_every_three_seconds`, `watcher_a_head_change_scan_mid_burst_leaves_no_redundant_capped_scan`, `watcher_debounce_cap_does_not_move_a_quiet_root`; real-time evidence in PR #6: 21 writes over 6 s → piles at 3.2 s and 6.4 s.)*

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

**D4 — Case-only rename on macOS.** Action: `mv f.txt F.txt`. Expect: git root and draft root alike: a delete+add pair (`f.txt` deleted, `F.txt` added — the scan lists names byte-exactly; git's own status may see no change on a case-insensitive FS, and that is why lastcall does not consult it); restore-deletion of `f.txt` refuses with `F.txt` named as the collision, because the filesystem resolves the name onto the existing entry (any fold rule: ASCII, Unicode case, NFD/NFC). *(H on macOS only; the collision tests skip visibly on a case-sensitive root.)*

**D5 — Unstaged rename.** Action: agent `mv d/old.rs d/new.rs` and edits 2 lines. Expect: one paired rename row "old.rs → new.rs" with 2-line hunk (similarity pairing via temp index), ledger stores delete + add. *(H)*

**D6 — Sparse checkout.** Setup: `git sparse-checkout set src/`. Expect: files outside `src/` do not appear as pending deletions — they carry the skip-worktree bit in the **user's** index, which the scan must consult (our private index knows nothing of it). *(H — verified)*

**D7 — Lockfile churn (collapsed class).** Action: `npm install` rewrites `package-lock.json` (4,000 lines). Expect: one collapsed row `package-lock.json (collapsed, +2,113/−1,887)` with single accept; no hunk view by default. *(E/U)*

**D8 — Binary and oversize.** Action: agent adds a 2 MB PNG and a 600 KiB generated file. Expect: both collapsed rows (binary; > `collapse_size_bytes`). *(E)*

**D9 — Nested repo.** Setup: `R/vendor/lib` is its own git repo. Action: agent edits `R/vendor/lib/x.c`. Expect: shows under root `R/vendor/lib`, not `R`; if `vendor/**` is in `ignore_globs` the watcher is slower but the next scan of that root still shows it. *(E)*

**D10 — Linked worktree.** Setup: `git worktree add ../R-wt feat-w`. Expect: `R-wt` is its own root with its own ledger/store (alternates → common dir); its HEAD watcher points at `.git/worktrees/R-wt/HEAD`; badge "worktree of R". Agent commits in `R-wt` → B1 behavior there only. *(H)*

**D11 — Paths with unicode and spaces.** Setup: `docs/résumé draft.md`. Expect: appears, accepts, restores correctly (all plumbing uses `-z`). *(H)*

**D12 — `search_depth` reads N folders down.** Setup: parent `P`; `P/a` (repo, one commit); `P/a/sub` (a git submodule of `a`, committed); `P/worktrees/b` (a clone of `a`); `P/worktrees/a-wt` (`git -C P/a worktree add ../worktrees/a-wt -b feat-w`); `P/deep/er/c` (repo); `P/node_modules/pkg` (repo); `P/link` (a symlink to `P/deep`). Expect at `search_depth = 1`: `[a]`. At 2: `[a, worktrees/a-wt (worktree of a), worktrees/b]`, every root filed under `P`; `a/sub` is not a root at any depth. At 3: adds `deep/er/c`. `node_modules/pkg` and anything through `link` are never listed at any depth. Restart at each depth: the same list. Back to 1: `a` only, the other ledgers still on disk. *(H for the listing at each depth; E for the badges, the parents, the restart and the ledgers. PROPOSED with Amendment v1.11, ratified by merging the Phase 10 PR.)*

**D13 — a worktree kept inside its repository.** Setup: parent `P`; `P/R` (repo, one commit) with `.worktrees/` in its committed `.gitignore`; `git -C P/R worktree add .worktrees/wt -b feat-w`. Expect at `search_depth = 1`: `[R]`. At 2: `[R, R/.worktrees/wt (worktree of R)]`, both filed under `P`; `R`'s pile holds nothing under `.worktrees/`. An agent edit in `wt` is pending in `wt` only (D10). Restart: the same list. Launched from inside `R` (parent inside a repository) at 2: the same two rows. A `.worktrees/` that is not ignored is listed at depth 1 already, through D9's nested path, with the same badge; the test covers that case too. *(E. PROPOSED with Amendment v1.11, ratified by merging the Phase 10 PR.)*

**D14 — Accepted on a branch, then the start branch checked out (the reported case).** Setup: `R` with 3 committed files on `main`; `git checkout -b future`; lastcall's first sight of `R` happens **here** (`seen = future's HEAD^{tree}`, `seen_branch = future`, no record for `main`). Action: an agent adds `n1`…`n4` (pure additions) and commits them; the user accepts the four files. Expect: pile `[]`; four overrides on `future`'s record. Action: `git checkout main`. Expect: `main`'s record is first-sighted from `future`'s: `main`'s tip is an ancestor of `future`'s tip, so the fold takes `main`'s committed content for `n1`…`n4` (absent) and drops their overrides; pile `[]`, **no deletions**; notice `switched future → main: first time here, seen state carried from future; 0 files pending`. Action: `git checkout future`. Expect: the parked record is back; pile `[]`; notice `switched main → future: 0 files differ from seen state`. Variant (the common case): first sight on `main`, then `checkout -b future` and the same actions; `main`'s own record is parked at the branch creation and restored at the return; pile `[]` at every step. Restart at each step: identical. *(H, E. PROPOSED with Amendment v1.12, ratified by merging the Phase 11 PR.)*

**D15 — An unattended run on a generated branch.** Setup: first sight on `main`, pile `[]`. Action: `git checkout -b run-1` (same commit; `run-1`'s record is a copy of `main`'s, the fold has nothing to do; notice `switched main → run-1 (same commit)`). An agent writes `a.rs` and `b.rs`, commits, writes `scratch.tmp` uncommitted, then `git checkout .` and `git clean -fd`, then `git checkout main`, leaving `run-1` in place. Expect on `run-1` after the writes and before the clean: pile `[a.rs, b.rs, scratch.tmp]` (the commit changes no worktree byte); after `git clean -fd` and before the return: `[a.rs, b.rs]` (the untracked file is gone from the worktree; lastcall's state lives outside the repository and is untouched). Expect after the return: `main`'s parked record, pile `[]`; the notice names 0 files. Action: a second run, `run-2`, the same way with `c.rs`; back on `main`: `[]`. Action: `git checkout run-1`. Expect: `run-1`'s parked record, pile `[a.rs, b.rs]`, the run's work waiting for review exactly as it was; accept-all there → `[]`; `git checkout main` → `[]`; `git checkout run-2` → `[c.rs]`. The ledger holds three records: `main` in force and two parked. Restart at each step: identical. *(H, E. PROPOSED with Amendment v1.12, ratified by merging the Phase 11 PR.)*

**D16 — Cherry-picks onto a named feature branch show once more, by design.** Setup: D15 after `run-1` and `run-2` were reviewed and accepted on their branches. Action: `git checkout -b feat/x main` (copy of `main`'s record), then `git cherry-pick main..run-1` and `git cherry-pick main..run-2` (new commit ids; neither run branch is an ancestor of `feat/x`). Expect: pile `[a.rs, b.rs, c.rs]`: the same content the user accepted on the run branches shows again here, because `feat/x`'s record was copied from `main`'s and hiding it would require seen-follows-content, which this phase does not do. Action: accept-all. Expect: `[]`; `feat/x`'s `seen_tree` is a fold; `main`'s record is untouched (`git checkout main` → `[]`). Restart: identical. *(H, E. PROPOSED with Amendment v1.12, ratified by merging the Phase 11 PR.)*

**D17 — The run in a linked worktree.** Setup: first sight on `main` in `R`; `git -C R worktree add ../R-run -b run-3`. Expect: `R-run` is its own root with its own ledger (D10); its first sight is `run-3`'s `HEAD^{tree}` (a root's first sight, not a branch's; `seen_branch = run-3`); `R`'s ledger has no `run-3` record. Action: the agent works and commits in `R-run`; the user reviews there and accepts; `git -C R-run checkout --detach` then `git -C R worktree remove ../R-run`. Expect: `R`'s pile `[]` throughout; `R`'s ledger unchanged. Action: `git -C R checkout run-3`. Expect: a first sight from `main`'s record, `run-3` is ahead, no fold: the run's committed files pending in `R`, over-show (the accepts made in `R-run`'s ledger are that root's, never shared). *(E. PROPOSED with Amendment v1.12, ratified by merging the Phase 11 PR.)*

**D18 — A branch that is ahead, never seen.** Setup: first sight on `main`; a coworker's `feat/other` exists locally with 2 commits touching `o1`, `o2`. Action: `git checkout feat/other`. Expect: a copy of `main`'s record, no fold (ahead, not an ancestor): pile `[o1, o2]`; notice `switched main → feat/other: first time here, seen state carried from main; 2 files pending`. Action: accept-all; `git checkout main`; `git checkout feat/other`. Expect: `[]` at each step; the second arrival's notice is the return form. Restart: identical. *(H, E. PROPOSED with Amendment v1.12, ratified by merging the Phase 11 PR.)*

**D19 — Detached HEAD and an operation in progress keep the record.** Setup: D18 after accept-all on `feat/other`. Action: `git checkout --detach HEAD~1`. Expect: no switch (`seen_branch` stays `feat/other`); pile `[o2]` as a deletion against the seen tree (the detach removed it from the worktree), over-show; notice `switched feat/other → <sha7>: 1 files differ from seen state` (the reflog line is `checkout: moving from …`, which classifies as a checkout, and a detached head labels as its short sha; `HEAD moved: …` is only the unclassifiable form). Action: an accept there lands in `feat/other`'s record; `git checkout feat/other` → no switch, `[]`. Action: `git rebase main` with a conflict stopped mid-way (the shape of `scenario_b10_rebase_stopped_on_conflict` in `test_integration_scenarios_b.rs`). Expect: `in_progress = rebase`, HEAD detached, the record in force unchanged, conflict markers pending; on `rebase --continue` finishing on `feat/other`: B5's rule, nothing re-flagged. Action: `git checkout --detach` then `git checkout -b feat/from-detached`. Expect: a first sight from `feat/other`'s record (the record in force), `A = refs/heads/feat/other`. Noted, not tested: a bisect detaches without an in-progress marker (notices not suppressed, as today); a rebase started from another branch files a mid-way accept on the starting branch (R4). *(E. PROPOSED with Amendment v1.12, ratified by merging the Phase 11 PR.)*

**D20 — Deleted, recreated, renamed.** Setup: D15 (three records). Action: `git branch -D run-1`; `git checkout run-2` (any switch). Expect: `run-1`'s parked record is pruned; `branches` holds `main` only while `run-2` is in force. Action: `git checkout -b run-1` from `main` (recreated). Expect: a first sight, a copy of the record just left; nothing from the old `run-1` survives. Action: `git branch -m run-2 run-two` while on `run-2`. Expect: `HEAD` names `run-two` and `refs/heads/run-2` is gone, so the record in force is re-labelled (R3's rename rule): `seen_branch = run-two`, no first sight, no parked record, the pile unchanged, no notice from the sync (the reflog gives whatever `git branch -m` writes; the test asserts the record, not the notice). Action: `git branch -m main trunk` while on `run-two` (a parked branch renamed). Expect: at the next switch `main`'s parked record is pruned; `git checkout trunk` is a first sight from the record in force, over-show by R2. *(H for prune and recreate, E for both renames. PROPOSED with Amendment v1.12, ratified by merging the Phase 11 PR.)*

**D21 — Restart, offline switches, and the 1.1 file.** Setup: D15. Action: kill lastcall on `main`; `git checkout run-1`; relaunch. Expect: the switch happens at open (`seen_branch` in the file says `main`, `HEAD` says `run-1`): `run-1`'s parked record in force, pile `[a.rs, b.rs]`, no notice (nothing moved while lastcall watched). Action: kill; `git checkout main`; relaunch: `[]`. Setup 2: a ledger written by a 1.1 binary (no `seen_branch`, no `branches`) on a root checked out on `feat/y`. Action: launch. Expect: `seen_branch = feat/y` adopted with no fold and no change to the pile; the file is stamped `1.2` on its next write, byte-identical until then. *(E; the 1.1 file as a fixture string in a ledger unit test too. PROPOSED with Amendment v1.12, ratified by merging the Phase 11 PR.)*

**D22 — Two processes over one root.** Setup: two engines opened on `R` (the `ops_commit_merges_with_a_ledger_written_by_another_engine` shape), first sight on `main`. Action: `git checkout -b feat/z`; engine 1 scans (performs the switch); engine 2 scans. Expect: engine 2 adopts the file (its `seen_branch` is already `feat/z`) and performs no second first sight; both piles equal. Action: engine 2 renders a row on `feat/z`; `git checkout main`; engine 1 scans (switch); engine 2 accepts the row it rendered **through `ops().accept_file` on the rendered row, without scanning first** (a scan would perform the switch and the accept would land, correctly, in `main`'s record). Expect: the accept is refused with `branch changed under this accept (now main); try again`, nothing written, engine 2's staged work dropped, engine 2's next scan shows `main`'s pile. Form: two engines opened over one state dir (`open_engine_with` twice, the way `Fresh::restart` opens its second), or a unit test in `ops.rs` over the crate-private `Harness` like `ops_commit_merges_with_a_ledger_written_by_another_engine`; the worker picks and names it. *(E. PROPOSED with Amendment v1.12, ratified by merging the Phase 11 PR.)*

**D23 — Uncommitted work and its accepted hunk survive `checkout -b` (B2 with an override).** Setup: first sight on `main`; `f1` has two hunks; accept hunk 1. Expect: `[f1]` with hunk 2 only. Action: `git checkout -b feat/w` (git carries the uncommitted `f1`). Expect: a copy, the override carried, pile `[f1]` with exactly hunk 2, the notice `switched main → feat/w (same commit)` (a first sight at the same commit keeps B2's form; the first-sight wording is for a moved head). Action: `git checkout main`. Expect: `[f1]`, hunk 2, the return notice `switched feat/w → main (same commit)`. *(H, E. PROPOSED with Amendment v1.12, ratified by merging the Phase 11 PR.)*

**D24 — The fold takes only what the record has accepted at the departed tip, never a blob for a path it holds as absent.** Setup: first sight on `main` at `c1`: `P = v1` (seen), `Q` absent, `f = v1`. The agent commits `c2` (`P = v2`, `Q = w1` added, `f = v2`) and `c3` (`P` and `Q` deleted, `f = v2`); the user accepts `f` only. Expect on `main` at `c3`: pile `[P deleted]` (`Q` is absent against an absent baseline; `f` accepted). Action: `git branch mid c2; git checkout mid`. Expect: `mid`'s tip is an ancestor of `main`'s and `P`, `Q`, `f` differ between the tips; `P`: baseline `v1` ≠ `main`'s tip (absent), not folded, shows as modified (`v1 → v2`); `Q`: baseline absent equals the tip's absent but `mid` has a blob, not folded, shows as added; `f` does not differ between the tips and is not considered; pile `[P, Q]`; notice `switched main → mid: first time here, seen state carried from main; 2 files pending`. Action: `git checkout main`. Expect: `[P deleted]`. Variant A: back on `main`, accept `P`'s deletion, then cut a second branch at `c2` (`git branch mid-a c2; git checkout mid-a`; `mid` already has a parked record, so a return there would be a load, not a first sight); `P`'s baseline is absent and `mid-a` has `v2`, so it shows as added; pile `[P, Q]`. Variant B (the positive fold): a fresh repository on `main` at `c1`; `git checkout -b future` before lastcall opens, so `future` is the root's first sight and `main` has no record; the agent commits `f = v2`, the user accepts `f`; `git checkout main` (`main`'s tip `c1` is an ancestor): `f`'s baseline `v2` equals `future`'s tip and `main` has `v1`, so it folds to `v1` and the override loses its blob; pile `[]`, notice `switched future → main: first time here, seen state carried from future; 0 files pending`; back on `future`: `[]`. *(H, E. PROPOSED with Amendment v1.12, ratified by merging the Phase 11 PR.)*

**D25 — A branch cut from the shared ancestor and committed to before lastcall looks (the sponsor's gate run).** Setup: first sight on `main` at `c1`; `git checkout -b feat` (observed, same commit); the agent commits `c2` adding `a`, `b`, `c`; the user accepts the three; pile `[]`. Action, in one shell line so that lastcall observes no intermediate state: `git checkout main && git checkout -b feat2 && <commit c3 adding d>`. Expect on `feat2`: a first sight from `feat`'s record, the record in force when the scan runs; the tips `c2` and `c3` have diverged with `M = c1`; `a`, `b`, `c` differ between `c2` and `c1`, their baselines (the accepted blobs) equal `feat`'s tip, so they fold to `c1`'s entries (absent) and their overrides are dropped; `d` is not in that set and keeps the copied baseline (absent), so it shows as added; pile `[d]`, **never** `a`, `b`, `c` as deletions; notice `switched feat → feat2: first time here, seen state carried from feat; 1 file pending`. Action: accept `d`; `git checkout feat` → `[]` (the parked record); `git checkout main` → a first sight from `feat2`'s record with `M = c1`, `main`'s own tip: `d` folds away; `[]`. Variant A (timing independence): the same sequence with a scan between every git command, D14's shape, gives the same pile at every step. Variant B (the hide the ancestor form admitted): D15 after `run-1` and `run-2` were accepted on their branches, lastcall on `run-2`; while lastcall is closed, `git checkout -b feat/x main && git cherry-pick main..run-2`; relaunch. Expect: the switch lands at open (`run-2` → `feat/x`, diverged, `M = main`'s tip): `c.rs` folds to absent because it was accepted at `run-2`'s tip, and the cherry-picked `c.rs` shows as added, pile `[c.rs]`, no notice (nothing moved while lastcall watched). Under the copy alone the carried override's blob equals the cherry-picked content and `c.rs` would have been hidden, seen-follows-content by accident, against D16. *(H, E. PROPOSED with Amendment v1.12, ratified by merging the Phase 11 PR.)*

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

**Phase 11 (Amendment v1.12, PROPOSED):** `lc.sh` gains `lc_branch_sync` (the record in force follows the branch named in `HEAD`; park to `branches/<enc name>/`, copy on first sight, then the fold onto the merge-base of the two tips: `merge-base`, `diff-tree` between the branch left and that commit, and the departed tip's `ls-tree` to tell accepted content from merely committed content), called first by every `lc_*` entry point that reads or writes state; D14, D15, D16, D18, D20, D23, D24 and D25 are its assertions, and B2, B3, B4 keep theirs. `scripts/harness/scenarios.sh` now runs **132 assertions**, all passing (49 before this phase).

**Status 2026-09-01:** `scripts/harness/scenarios.sh` (run: `bash scripts/harness/scenarios.sh`; fixtures go to `scripts/harness/work/`, gitignored, or `$LC_WORK`) — 49 assertions, all passing. Scenarios marked *verified* above ran green; the remaining H scenarios (A3 property, A6, D2 restore, D5 pairing, E1/E3/E4) are Phase 2 tests.

The pre-Phase-2 harness implements only the plumbing the engine will call — no TUI, no watcher, no ledger JSON — as shell functions: `lc_init_store R` (bare store + alternates), `lc_first_sight`, `lc_scan` (private index refresh + `diff-files` + `ls-files --others`), `lc_accept_file`, `lc_accept_all` (index-info + write-tree), `lc_upstream_paths` (rev-list `--not --remotes`, `diff-tree --cc` for merges), and `lc_pile` (composes the expected output). Every **H** scenario above is one function call sequence with an `assert_pile` at each Expect line. A failing assertion is a spec bug to fix here first, not a test to loosen.
