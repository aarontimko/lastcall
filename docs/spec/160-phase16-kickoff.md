# Phase 16 Kickoff Prompt (operational artifact, not design)

**Rulings: the sponsor's 2026-10-04 rulings, verbatim below; every other call in this file is the orchestrator's under the sponsor's 2026-09-05 delegation, listed at close-out.**

**The report (sponsor, 2026-10-04, paraphrased):** after a pull request of theirs was squash-merged on GitHub and pulled with `git pull --ff-only`, about 20 files they had accepted 10 to 15 minutes earlier came back as plain pending rows. Pulls of the same kind usually fold, "sometimes ... but not other times".

---

## Mission

Build Phase 16 per this file and Amendment v1.18 (PROPOSED, §6.2 and §6.4). One deliverable: **a deleted branch's accepted content keeps folding into `[seen]`**. When a switch prunes the record of a branch that no longer exists, the record is **retired** instead of dropped: kept, for a bounded time, only so the seen-elsewhere annotation can still match it. Engine (`ledger.rs`, `ops.rs`, `seen.rs`), tests, docs. One construction run on the branch `fix/seen-after-branch-delete` in the main checkout; no release on this branch.

## The rulings

1. **The design (2026-10-04, "1 rec"):** keep today's upstream identity rule and fix the real gap, the deleted branch's record, in place of the committer-only upstream test built first (branch `fix/upstream-self-authored-merges`, never pushed, abandoned). The committer-only test grouped every squash-merged pull request as `[upstream]`, including work never accepted, which the sponsor uses in greenfield work to see pull requests they did not review before the merge: "it would be nice to be able to see those if we can get it reliable enough".
2. **The reach (2026-10-04, "1 rec yes it may be the deleted branches of why this sometimes happens but not other teims --- a simple retention makes sense"):** the same folder (root) only; matching across the folders of one repository (worktrees) is not built.

## The diagnosis (facts at `3b2c28b`, reproduced in a scratch copy)

A squash commit GitHub made for the user's pull request carries the user as author and GitHub as committer. The v1.0 upstream test counts it as the user's (author matches), so its paths are plain rows, and the seen annotation (`seen.rs`, Amendment v1.15) compares each with every **parked** record. Three shapes, the first sight on `main`, `run-1` committed and accepted, pushed, squashed on the server, then:

| Shape | Result |
|---|---|
| `checkout main`, `pull` (`run-1` kept) | `a.rs`, `b.rs` fold into `[seen]` on `run-1` |
| `checkout main`, `branch -D run-1`, `pull` | plain rows, no group: **the report** |
| `checkout main`, `pull`, then `branch -D run-1` | `[seen]` (the record is pruned only at the next switch) |

`gh pr merge --delete-branch` switches to the default branch and deletes the local branch in one command, which is the second shape: the switch prunes `run-1`'s record (`prune_parked`, `ops.rs:2246`, called from the switch at `ops.rs:1984` and the rename at `ops.rs:1940`) before the pull brings the content. Whether the branch is deleted before or after lastcall sees the switch decides the result, which is the "sometimes".

## What exists (facts at `3b2c28b`)

- `Ledger::branches: BTreeMap<String, BranchRecord>` (`ledger.rs:508`); `BranchRecord { seen_tree, seen_at, overrides, undo, parked_at }` (`ledger.rs:439`); on the wire `branches` is raw JSON per record, omitted when empty (`LedgerWire`, `ledger.rs:477`). `SCHEMA_VERSION = "1.2"`. A field added under a schema without a bump has precedent: v1.11's `undo` and `snoozed_until` under 1.1.
- `prune_parked` drops every parked name with no `refs/heads/<name>` (`for-each-ref`); a failed listing skips the prune (fail open).
- `seen::mark` (`seen.rs`) iterates `ledger.branches`, skips the record in force, composes a path's baseline from the record's override (a blob with its mode; seen-as-absent or flag-only matches nothing) else its seen tree entry, and pushes the branch name onto `row.seen_on` on an exact oid and normalised mode match; rows with an annotation (`upstream`, `mixed`) are never candidates. `SeenCache` holds one listing per `(branch, tree oid)` and is pruned to the records still parked at that tree. The engine's listing closure (`engine.rs:1395`) lists a tree the store no longer has as empty.
- `fold_onto_first_sight` (R2) and the harness twin (`lc.sh`, "the parked records to ask") consult parked records; compaction and undo act on the record in force only.
- Tests that pin the prune: `seen_group_a_deleted_branch_matches_until_the_next_switch` (`test_integration_seen_group.rs`, its tail asserts no marks after the prune) and `scenario_d20_deleted_recreated_renamed` (D20: the prune empties `branches` of `run-1`; a recreated `run-1` is a first sight with nothing of the old record).
- The harness (`scripts/harness/lc.sh`) has no seen-elsewhere annotation; `lc_branch_prune` deletes the parked directory.
- `FixedClock` (`ledger.rs:81`) for tests; `Ops` carries `clock`.

## The model

1. **Retire, never drop.** Where `prune_parked` removes a parked record today, it moves it to a new `Ledger::retired: Vec<RetiredRecord>`, `RetiredRecord { branch, retired_at, seen_tree, overrides }` (the `undo` stack and `seen_at` are not kept: nothing reads them once the branch is gone). `retired_at` is the clock's now.
2. **One per name.** Retiring a name that is already retired replaces the older entry.
3. **Retention.** An entry is dropped when it is older than **30 days** (`RETIRED_DAYS`), and the list holds at most **20** entries (`RETIRED_CAP`), the oldest by `retired_at` dropped first. Both are applied at every prune, so they cost nothing between switches; an entry can outlive 30 days on a root that never switches again, which is accepted content and harmless.
4. **Matching.** `seen::mark` compares retired records after parked ones, with exactly the parked rule (override blob and mode, else the tree entry; seen-as-absent and flag-only match nothing). A match pushes the retired branch's name onto `seen_on` unless that name is already there (a recreated branch of the same name that also matches); `seen_on` stays sorted. The cache keeps retired listings the same way it keeps parked ones and drops them when the entry leaves the list. A retired tree the store no longer has lists as empty (the existing closure): nothing matches, no notice.
5. **Nothing else reads a retired record.** It is never the record in force (a recreated name is a first sight: D20 unchanged), never asked by the fold at first sight, never compacted, never undone, and not listed in `status --json`'s `parked_branches`. The `[seen]` header names the branch as it was, as it does today for a deleted branch before the prune.
6. **Wire.** A top-level `retired` array after `branches`, omitted when empty, so a ledger that never retired anything rewrites byte-identical; the schema stays `1.2`. Each entry parses strictly; an unreadable one is dropped at load with a notice, like a parked record. A `v0.7.0` binary ignores the field and drops it at its next write, which loses only the fold (an over-show).
7. **No user-facing switch, no config key, no `status` change, no TUI change.**

## Tests first

Red at `3b2c28b` where the behaviour is new, then green:

- **D29, the report** (`test_integration_seen_group.rs` or the D suite): first sight on `main`; `run-1` commits `a.rs`, `b.rs`, accepted; pushed; on the server a squash commit with the user as author and GitHub (`GitHub <noreply@github.com>`) as committer, pushed to `main`; `checkout main`, `branch -D run-1`, `pull --ff-only`. Expect `[seen]` with `a.rs`, `b.rs` on `run-1`; a group accept empties the pile. Variant: one file edited by the server commit (content differs) stays a plain row. Variant: a coworker's squash commit (coworker author and committer) bringing identical content stays `upstream` (annotated rows are never candidates).
- **The flipped test:** `seen_group_a_deleted_branch_matches_until_the_next_switch` keeps its first half; after the switch the marks remain, from the retired record (renamed to say so).
- **D20 unchanged** in what it asserts (`branches`, first sight of the recreated name); add: `retired` holds `run-1` after the prune, and the recreated `run-1`'s record carries nothing of it.
- **Retention:** with a fixed clock, an entry 31 days old is dropped at the next switch and its content no longer folds; 21 retirements keep the 20 newest; retiring a name twice keeps one entry, the newer.
- **Ledger unit tests:** wire round trip with retired entries; byte-identical rewrite with none; an unreadable entry dropped with a notice and the rest kept; a `1.2` file without the field loads.
- **`seen.rs` unit tests:** a retired match, a retired seen-as-absent and flag-only override matching nothing, a name both parked (recreated) and retired giving one `seen_on` entry, a missing retired tree giving no marks and no error.
- **The oracle:** the seen-oracle proptest (`seen_oracle` or its current name) still passes at 64 cases; if it models parked records, it models the retirement too.

## Docs (same commits)

- `docs/dev/engine.md`: the pipeline step (line ~148), the switch section (line ~611), the `jq` recipes (`.retired`), the fail-open table row for a retired tree.
- `docs/review-loop.md`: the `[seen]` paragraph names a pull request squash-merged on GitHub and pulled; "deleting a branch drops what it remembered" becomes what is kept and for how long. House style: no em-dashes, no process vocabulary.
- `CHANGELOG.md`: `## Unreleased` / `### Fixed`: files you accepted on a branch fold into `[seen]` after its pull request is squash-merged and pulled, even when the branch was deleted first.
- `docs/spec/01-scenarios.md`: D29 PROPOSED; a PROPOSED note under D20.

## Spec amendments carried by the PR (Amendment v1.18, PROPOSED)

§6.2: the additive `retired` field (model 1, 3, 6). §6.4 seen-elsewhere bullet: "a parked record that outlives its branch (pruning happens at a switch) keeps matching until the prune" becomes: after the prune it keeps matching as a retired record for 30 days (at most 20 kept). §11 "Parked records outlive their branch until the next switch" rewritten (the window is now 30 days; the stale-name hardening stands). §10 entry for the rulings and the abandoned committer-only design, with the reason. Amendments line v1.18.

## Gate

- [ ] D29 red at `3b2c28b`, green after; the flipped test; D20's addition; retention, ledger and `seen.rs` tests as listed.
- [ ] `just lint`, `just test-unit`, `just test-scenarios`, `just test-prepush`, `just harness` green; `just golden-update` not needed (no `status` change).
- [ ] Docs and amendment v1.18 as listed; house style holds.
- [ ] Adversarial code review run, findings triaged.
- [ ] Amendment v1.18 ratified by the sponsor merging the PR.
