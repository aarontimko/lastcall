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
| `checkout main`, `branch -D run-1`, `pull`, no scan between the checkout and the delete | plain rows, no group: **the report** |
| `checkout main`, `pull`, then `branch -D run-1`, a scan between the checkout and the delete | `[seen]` (the record is pruned only at the next switch) |

`gh pr merge --delete-branch` switches to the default branch and deletes the local branch in one command, which is the second shape: the switch prunes `run-1`'s record (`prune_parked`, `ops.rs:2246`, called from the switch at `ops.rs:1984` and the rename at `ops.rs:1940`) before the pull brings the content. Whether lastcall scans between the checkout and the delete decides the result, in either order (review F2: `checkout; pull; branch -D` with no scan between loses the record the same way), which is the "sometimes". A different shape, not this report: with GitHub's email privacy on, the squash commit's author is `<id>+<login>@users.noreply.github.com`, which matches neither email, so the whole squash is `[upstream]` (grouped, never plain), as ruling 1 wants for work nobody here accepted (review F7).

## What exists (facts at `3b2c28b`)

- `Ledger::branches: BTreeMap<String, BranchRecord>` (`ledger.rs:508`); `BranchRecord { seen_tree, seen_at, overrides, undo, parked_at }` (`ledger.rs:439`); on the wire `branches` is raw JSON per record, omitted when empty (`LedgerWire`, `ledger.rs:477`). `SCHEMA_VERSION = "1.2"`. A field added under a schema without a bump has precedent: v1.11's `undo` and `snoozed_until` under 1.1.
- `prune_parked` drops every parked name with no `refs/heads/<name>` (`for-each-ref`); a failed listing skips the prune (fail open).
- `seen::mark` (`seen.rs`) iterates `ledger.branches`, skips the record in force, composes a path's baseline from the record's override (a blob with its mode; seen-as-absent or flag-only matches nothing) else its seen tree entry, and pushes the branch name onto `row.seen_on` on an exact oid and normalised mode match; rows with an annotation (`upstream`, `mixed`) are never candidates. `SeenCache` holds one listing per `(branch, tree oid)` and is pruned to the records still parked at that tree. The engine's listing closure (`engine.rs:1395`) lists a tree the store no longer has as empty.
- `fold_onto_first_sight` (R2) and the harness twin (`lc.sh`, "the parked records to ask") consult parked records; compaction and undo act on the record in force only.
- Tests that pin the prune: `seen_group_a_deleted_branch_matches_until_the_next_switch` (`test_integration_seen_group.rs`, its tail asserts no marks after the prune) and `scenario_d20_deleted_recreated_renamed` (D20: the prune empties `branches` of `run-1`; a recreated `run-1` is a first sight with nothing of the old record).
- The harness (`scripts/harness/lc.sh`) has no seen-elsewhere annotation; `lc_branch_prune` deletes the parked directory.
- `FixedClock` (`ledger.rs:81`) for tests; `Ops` carries `clock`.

## The model

1. **Retire, never drop.** Where `prune_parked` removes a parked record today, it moves it to a new `Ledger::retired: BTreeMap<String, RetiredRecord>` keyed by branch name, `RetiredRecord { retired_at, seen_tree, overrides }` (the `undo` stack and `seen_at` are not kept: nothing reads them once the branch is gone; overrides are kept whole, because a flag-only or seen-as-absent override is what stops a path matching). `retired_at` is the clock's now.
2. **One per name.** Retiring a name that is already retired replaces the older entry (the map's insert).
3. **Retention.** An entry is dropped when it is older than **30 days** (`RETIRED_DAYS`); an unparsable `retired_at` reads as expired (the `snooze_active` rule), so a hand-edited value never keeps an entry forever. The map holds at most **20** entries (`RETIRED_CAP`), the oldest by `retired_at` dropped **at insertion**, so a run of failed `for-each-ref` listings can never grow it past the cap. The age check runs every time `prune_parked` runs, independent of the listing: its early return becomes `branches.is_empty() && retired.is_empty()`, and a failed listing still skips the parked half only (review F5). An entry can outlive 30 days on a root that never switches again, which is accepted content and harmless.
4. **Matching.** `seen::mark` compares retired records after parked ones, with exactly the parked rule (override blob and mode, else the tree entry; seen-as-absent and flag-only match nothing). A retired record whose name is the branch in force is not compared, as a parked one is not (review F3: a recreated name is a first sight; at worst an over-show). A match pushes the retired branch's name onto `seen_on` unless that name is already there (a name both parked and retired); `seen_on` stays sorted. `SeenCache` is keyed by **tree oid** with a retain-set of the oids still referenced by a parked or retired record, so R2 copies sharing one tree are listed once (review F8). A retired tree the store no longer has lists as empty (the existing closure): nothing matches, no notice.
5. **Nothing else reads a retired record.** It is never the record in force (a recreated name is a first sight: D20 unchanged), never asked by the fold at first sight, never compacted, never undone, and not listed in `status --json`'s `parked_branches`. The `[seen]` header names the branch as it was, as it does today for a deleted branch before the prune.
6. **Wire.** A top-level `retired` object (name to record, raw JSON per record as `branches` is) declared in `LedgerWire` between `branches` and `snoozed_until` (`undo` stays last), omitted when empty, so a ledger that never retired anything rewrites byte-identical; the schema stays `1.2`. Each entry parses strictly; an unreadable one is dropped at load with a notice, like a parked record. A `v0.7.0` binary ignores the field and drops it at its next write, which loses only the fold (an over-show).
7. **No user-facing switch, no config key, no `status` change, no TUI change.** The harness (`scripts/harness/lc.sh`) does not change: it has no seen annotation, and `lc_branch_prune` keeps deleting the parked directory, which stays true for `branches`.

## Tests first

Red at `3b2c28b` where the behaviour is new, then green:

- **D29, the report** (`test_integration_seen_group.rs` or the D suite): first sight on `main`; `run-1` commits `a.rs`, `b.rs`, accepted; pushed; on the server a squash commit with the user as author and GitHub (`GitHub <noreply@github.com>`) as committer, pushed to `main`; `checkout main`, `branch -D run-1`, `pull --ff-only`, with **no scan between the checkout and the delete** (otherwise it is green today, review F2). Variant: the same with a scan between, and `checkout; pull; branch -D` with no scan until the end: all three give the same pile. Expect `[seen]` with `a.rs`, `b.rs` on `run-1`; a group accept empties the pile. Variant: one file edited by the server commit (content differs) stays a plain row. Variant: a coworker's squash commit (coworker author and committer) bringing identical content stays `upstream` (annotated rows are never candidates).
- **The flipped test:** `seen_group_a_deleted_branch_matches_until_the_next_switch` keeps its first half; after the switch the marks remain, from the retired record (renamed to say so).
- **D20 unchanged** in what it asserts (`branches`, first sight of the recreated name); add: `retired` holds `run-1` after the prune, and the recreated `run-1`'s record carries nothing of it.
- **Retention:** with a test-local `Clock` over a `Mutex<SystemTime>` passed through `Fresh::with(.., EngineOptions { clock: .. }, ..)` (`FixedClock` is immutable and `Fresh`'s options are private, review F6), so the `retired_at` stamp is tested too; an entry 31 days old is dropped at the next switch and its content no longer folds; 21 retirements keep the 20 newest; retiring a name twice keeps one entry, the newer.
- **Ledger unit tests:** wire round trip with retired entries; byte-identical rewrite with none; an unreadable entry dropped with a notice and the rest kept; a `1.2` file without the field loads.
- **`seen.rs` unit tests:** a retired match, a retired seen-as-absent and flag-only override matching nothing, a name both parked (recreated) and retired giving one `seen_on` entry, a retired name equal to the branch in force not compared, a missing retired tree giving no marks and no error, two records sharing a tree listed once.
- **The oracle** (`test_integration_seen_oracle.rs`) gains **Property 1b**: inside `World::scan`, for every row with a non-empty `seen_on`, the disk entry is in `Seen(path)`. Today the oracle checks only absent paths and `path:change`, so a wrong mark is invisible to it; the reviewer ran 1b green at 64 cases at `3b2c28b`. Retirement is then covered by the generator's `DeleteBranch` then `Checkout` (review F1).

## Docs (same commits)

- `docs/dev/engine.md`: the seen-annotation paragraph (~546 to 550, and the step 9 text at ~148 that describes the marks), the switch section (~605 to 615), the `jq` recipes (`.retired`), the fail-open table (~700 to 712) row "a retired tree the store no longer has: lists as empty, no marks, no notice". `docs/dev/bench.md`: one line on the cache bound (at most 20 retired listings beside the parked ones, shared per tree oid).
- `docs/review-loop.md`: the `[seen]` paragraph names a pull request squash-merged on GitHub and pulled; "deleting a branch drops what it remembered" becomes what is kept and for how long. House style: no em-dashes, no process vocabulary.
- `CHANGELOG.md`: `## Unreleased` / `### Fixed`: files you accepted on a branch fold into `[seen]` after its pull request is squash-merged and pulled, even when the branch was deleted first.
- `docs/spec/01-scenarios.md`: D29 PROPOSED; a PROPOSED note under D20.

## Spec amendments carried by the PR (Amendment v1.18, PROPOSED)

§6.2: the additive `retired` field (model 1, 3, 6). §6.4 seen-elsewhere bullet: "a parked record that outlives its branch (pruning happens at a switch) keeps matching until the prune" becomes: after the prune it keeps matching as a retired record for 30 days (at most 20 kept). §11 "Parked records outlive their branch until the next switch" rewritten: a deleted name in the `[seen]` header for up to 30 days is now the feature; the hardening becomes a `(deleted)` marker on retired names in the header, trigger = the sponsor confused by a deleted name there (review F4). §10 entry for the rulings and the abandoned committer-only design, with the reason. Amendments line v1.18.

## Gate

- [x] D29 red at `3b2c28b`, green after; the flipped test; D20's addition; retention, ledger and `seen.rs` tests as listed. *Red at `e9c7f17` (this kickoff only on top of `3b2c28b`), green at `c1e82a8`.*
- [x] `just lint`, `just test-unit`, `just test-scenarios`, `just test-prepush`, `just harness` green; `just golden-update` not needed (no `status` change).
- [x] Docs and amendment v1.18 as listed; house style holds.
- [x] Adversarial code review run, findings triaged. *F1 to F8; see below and §10 2026-10-04.*
- [x] Amendment v1.18 ratified by the sponsor merging the PR. *PR #47, `ed7da4d`, 2026-10-04.*
- [ ] `v0.7.1` released by the tag on the release pull request's merge commit (the fix was merged without the bump; the sponsor's "1 rec", 2026-10-04): ticked by the next PR to `main`, with the tag object and the release run id.

## Design review, folded (2026-10-04)

A fresh-context adversarial design review (Fable), probes in a scratch copy only: the diagnosis table reproduced (8 of 8 probes as predicted), `LedgerWire` confirmed to ignore unknown fields and the v1.11 precedent confirmed; no blocker; verdict safe to build once F1 to F5 were in the text. Folded: F1 (the oracle's Property 1b, the only instrument that would catch a wrong retired mark), F2 (D29 withholds the scan, plus the order variants), F3 (a retired name in force is not compared), F4 (§11 hardening restated), F5 (retention independent of the listing; cap at insertion; unparsable `retired_at` is expired), F6 (the clock route named), F7 (the noreply-author shape named in the diagnosis), F8 in part (the cache keyed by tree oid), F9 (a map keyed by name), F10 (doc pointers; the harness unchanged), F11 (wire placement). **Rejected in part, F8:** dropping flag-only and seen-as-absent overrides at retirement. They are what stops a path matching (model 4), so dropping them would let the path fall through to the tree entry and fold content the record marked as noted or deleted; the ledger bound (the compaction threshold times 20) is accepted.

## Code review, folded (2026-10-04)

A fresh-context adversarial code review (Fable) of `c1e82a8`, `b0ddbdd` and `26d1442`, probes in a scratch copy only: every red claim re-run at the old engine, the seen oracle at 256 cases, two engines over one root and a recreated name probed; verdict safe to push, no gating finding. Fixed: F1 (a `retired_at` in the future now expires, `be15e8e`), F2 (the user docs say the 30 days end at the first branch switch after them), F3 (the review loop's cherry-pick sentence), F4 (the new changelog entry's wording; the 0.3.0 line kept as shipped), F5 (the memory estimate in `bench.md`), F6 (the engine notes' ledger layout line). Deferred: F7, a branch deleted and recreated between two scans gets its old record back, behaviour since Phase 11 (§11). Rejected: F8, a retired record with no tree takes a cap slot, negligible.
