# Phase 15 Kickoff Prompt (operational artifact, not design)

**Rulings: the sponsor's 2026-09-27 priority (§10 2026-09-27, "the release flow") and the 2026-09-26 rulings on range selection and the seen-oracle case (§10 2026-09-26, "the walks" and "the release"); every other call in this file is the orchestrator's under the sponsor's 2026-09-05 delegation, labelled where it is a judgment call.** **Adversarial design review: pending** (folded findings are listed at the end of this file when it is done).

**The framing (sponsor, 2026-09-27, right after `v0.6.0`):** three pull requests and three CI runs to release one feature set is too much; reducing it is the first item. Behind it, the three items the `v0.6.0` close-out carried: shift-click range selection in the left pane, the latent seen-oracle case, and the PTY scene that loses a key after `Esc` on a slow runner. The entry item is the sponsor's cold read of the `docs/config.md` section that Deliverable I of Phase 14 wrote.

---

## Mission

Build Phase 15 per `00-spec.md` §8 "Phase 15", the §10 entries named above and Amendment v1.17 (PROPOSED, §6.3). Four deliverables, in the order they are built:

- **C. The fold takes the path the branch just left did not finish** (engine: `ops.rs` `fold_onto_first_sight`; the harness twin `scripts/harness/lc.sh`; `docs/dev/engine.md`; `01-scenarios.md` D24 PROPOSED paragraph). One guard removed; the six-operation history from the `v0.6.0` close-out as a fixed-shape test, red first.
- **D. A PTY scene never sends a bare `Esc` a key can merge into** (test harness only: `crates/lastcall/tests/test_e2e_tui_pty.rs`; `docs/dev/tui.md` PTY section).
- **B. Range selection in the left pane** (TUI: `input.rs`, `app.rs`, `render.rs`, `run.rs`; `crates/lastcall-testkit/src/pty_tui.rs`; docs). A contiguous run of file rows in one repository, extended by `shift-j`/`shift-k` or a shift-click, accepted as one by `shift-a` through `accept_group`, undone as one by `z`.
- **A. One pull request per release** (`scripts/release.py`, `justfile`, `AGENTS.md`, `docs/dev/operations.md`, the kickoff convention). The version bump and the dated CHANGELOG heading are made on the phase branch by `just release-prep`; the tag is cut from the phase PR's merge commit; nothing after the tag needs a pull request of its own.

Each deliverable is one construction run on the branch `feat/phase15` in the main checkout, C with D first (engine and harness, small), then B (the TUI, the largest), then A (the release script, last, because the phase's own release exercises it: the orchestrator runs `just release-prep 0.7.0` on this branch as the final commit before the push).

## The rulings

1. **A, the sponsor's priority (2026-09-27), verbatim:** "we really need to reduce the amount of PRs for this - 3 PRs and CIs to release a feature is overkill.... let's add this as a top priority after we're done with this". This supersedes the 2026-09-17 ruling that the release is its own five-file pull request (§10 2026-09-17). The design below is the orchestrator's Rec; the sponsor rules on it at kickoff.
2. **B, range selection (2026-09-26, "1 rec"):** click, shift-click in the left pane to select a run of rows and accept them at once, on `accept_group`, with the modifier crossterm already delivers.
3. **C, the seen-oracle case (2026-09-26, "1 rec"):** the shrunk history as a fixed-shape failing test first, then the fix.
4. **D, the `Esc` race (§11, 2026-09-27):** `select_until` proves the `Esc` was consumed before the next key.

**Orchestrator judgment calls, labelled as such** (all two-way doors; listed again at phase close): the range's keyboard keys (`shift-j`, `shift-k`, plus `shift-down`, `shift-up`) and action names (`extend_down`, `extend_up`); a range holds file rows only, inside one repository, never a group row or an open seen group's members; `a` on a range refuses with the file key named; a plain move, a plain click or `back` clears the range; the confirm threshold applies to a range as to every other scope; the completion status reuses the repository wording; the release script keeps its three verbs, `prep` moves to the branch; the after-the-tag facts ride in the next phase's kickoff.

## Entry baseline (inherited obligations)

- **The sponsor's cold read of `docs/config.md` `## Which key leaves what out`** (the Phase 14 I line's `[sponsor]` half, still open in §8): the sponsor reads the section cold on the released `v0.6.0` docs and says whether it answers, without the orchestrator's help, which key leaves what out. Recorded in §10 with the sponsor's words; a confusion found there is a docs fix inside this phase.
- §11 "A seen-oracle random history manufactures a pending row": closed by C.
- §11 "A PTY scene loses a key after `Esc` on a slow runner": closed by D.
- Unit floor: 928 (`just test-unit` on `dd069a7`); integration 152; e2e 124; scenarios 98; prepush 175.

## Entry preconditions (orchestrator-confirmed before launch)

- `main` at `dd069a7` (PR #45 merged), `v0.6.0` on `aa69c93`, tree clean, tiers green as above.
- No `## Unreleased` section in `CHANGELOG.md` (the first commit that changes behaviour creates it).
- The sponsor's own `~/.config/lastcall/config.toml` still says `review_ignored`: the release binary refuses to launch until renamed; that rename is the sponsor's and is not a phase item.

## Deliverable C: the fold takes the path the branch just left did not finish

### What exists (facts at `dd069a7`)

- The proptest `seen_oracle_no_hide_and_no_manufactured_work_over_random_branch_histories` (`crates/lastcall-engine/tests/test_integration_seen_oracle.rs:578-589`) runs an operation list twice, once scanning after every operation and once scanning only at `AcceptAll` and at the end, and asserts every row of the second run is on some screen the first run showed (`no_row_manufactured`, lines 532-558; the `MANUFACTURED:` assertion at 545-556). `failure_persistence: None` (line 89); cases from `PROPTEST_CASES` (8 by default, 64 in `just test-prepush`, `justfile:79-80`).
- The shrunk input `[Commit([(1, Some(0))]), Cut(Tip), AcceptAll, Commit([(1, None)]), Checkout(2), Cut(Older)]` reads: write `f2 = "v0\n"` and commit on `main`; branch `b0` at the tip; accept all (`f2 = v0` becomes seen state on `b0`); delete `f2` and commit on `b0`; check out `main`; branch `b1` at `main~1`, the seed commit (`Op` enum lines 127-155; `PATHS` line 63, index 1 is `f2`; `BLOBS` line 65).
- The walk through `RootState::sync_branch` (`engine.rs:329-382`) and `Ops::switch_branch` (`ops.rs:1889-1991`): the unobserved run arrives on `b1` carrying `b0`'s record (`f2 = v0` accepted, then deleted without an accept). `fold_onto_first_sight` (`ops.rs:2042-2228`) computes the merge base `c0`, the seed commit, finds `f2` differs, and refuses it at **guard 1** (`if base != tip_a { continue; }`, `ops.rs:2170-2174`) because the record's entry `v0` is not the departed tip's entry (absent). Guard 2 (`ops.rs:2176-2182`, the merge-base entry must be seen state: `first_sight_covers`, 2117-2125) would have passed, since `c0` is the first-sight head. The copy keeps `f2 = v0`, the disk holds the seed's `b`, and the scan lists `f2:Modified`. The observed run copied `main`'s untouched record on the same switch and shows nothing.
- Guard 1 predates guard 2 (the seen-state target of 2026-09-16, §10; `docs/dev/engine.md:576-590` "only when both of two things hold"). Its stated reason, content the user never looked at on the departed branch must not become seen state on the arriving one, is what guard 2 enforces: the entry that becomes seen state is the merge-base entry, never the departed tip's, and the departed branch's record is parked whole (`ops.rs:1949-1955`, 1978), so the unaccepted deletion shows as `f2:Deleted` on the next return to `b0`.
- The harness twin carries the same guard: `scripts/harness/lc.sh:200-201` (`[ "$base" = "$tipa" ] || continue`), comments at 155-156 and 176-179.
- Frozen text naming the guard: `00-spec.md:395` ("folding the paths finished on the left branch"); `01-scenarios.md:119` D24's title ("The fold takes only what the record has accepted at the departed tip"). D24's expectations do not change: its refusals are guard 2 refusals (the merge-base entries are past the first-sight head and in no record).

### The model

1. Guard 1 is removed from `fold_onto_first_sight` and from `lc.sh`; the departed tip's tree read (`ops.rs:2103-2113`) goes with it. A path whose record entry already equals the merge-base entry is skipped as a no-op (no tree write).
2. The docstring (`ops.rs:2003-2010`) and the clause (b) comment (`2131-2135`) say the one rule: a path folds when its merge-base entry is seen state (first sight covers it, or a record holds it), whatever the departed tip holds, because the departed branch keeps its own record.
3. Amendment v1.17 (PROPOSED, §10 and the amendments list): §6.3 line 395's clause reads "folding the paths whose merge-base entry is seen state"; D24 gets a PROPOSED paragraph saying its title's "at the departed tip" is no longer a condition and its expectations stand. `docs/dev/engine.md:576-590` rewritten to the one rule.
4. Every existing fold refusal stays a refusal (each is also a guard 2 refusal): `ops_switch_branch_does_not_fold_content_the_record_never_saw` (`ops.rs:2730`), `..._refuses_the_fold_when_the_entry_is_seen_state_nowhere` (2988), `..._when_a_parked_record_has_seen_nothing` (3034), `..._never_folds_through_an_unknown_first_sight_head` (3092), `scenario_d24_*` (`tests/test_integration_scenarios_d.rs:2129-2220`), `scenario_d26_*` (2401-2558), the harness assertions (`scripts/harness/scenarios.sh:226-239`). If one flips, that is a finding for the report, not a test to edit.

### Tests first (C)

- `seen_oracle_a_path_left_unfinished_on_the_departed_branch_still_folds` beside `seen_oracle_the_seed_folds_back_at_the_first_sight_head` (`test_integration_seen_oracle.rs:615`): the six operations through `both_runs`, both piles empty. Red on the unchanged code with `MANUFACTURED: f2:Modified`; the red output in the report.
- A unit test in `ops.rs` beside `ops_switch_branch_folds_when_the_arrival_is_an_ancestor` (2667) with the same shape at the `Ops` level: accept on the branch, a later unaccepted commit to the same path, switch to a branch at the first-sight head, the path folds and the record left behind keeps the unaccepted change.
- `just test-scenarios` and `just harness` green after the `lc.sh` change; the D suite counts in the report.

## Deliverable D: a PTY scene never sends a bare `Esc` a key can merge into

### What exists (facts at `dd069a7`)

- `select_until` (`crates/lastcall/tests/test_e2e_tui_pty.rs:1301-1331`) sends `\x1b` unconditionally, then waits up to 400 ms for the header text; when the header is already on screen the wait returns at once, before the program has read the byte. The next `send` in the caller then lands in the same read and crossterm parses `Esc` plus the key as `Alt-<key>`. The macOS `integration` job of the `v0.6.0` release ci run failed exactly so (`pty_seen_group_cherry_pick_expand_flag_accept_undo`, the third `A` lost; §11 2026-09-27). 22 call sites.
- The help-overlay scene guards the same coalescing by waiting for the overlay to close (`test_e2e_tui_pty.rs:5166-5178`); the reload scene guards it with a 50 ms sleep (`5557-5558`); the other bare `Esc` sends (2391, 2559, 2953, 4249, 4433) each wait for the modal they close.
- `back` is bound to `esc`, `h` and `left` (`input.rs:476`); `h` is not an escape prefix. In the nav `back` does nothing.
- The hint line shows the focus (`↑↓ select  ⏎ open` with the nav focused, `↑↓ scroll  ← back` with the diff focused; `docs/dev/tui.md`, the hint line row), but not while a status message owns the bottom row (`render.rs:501-540`), which is the case in the failing scene.

### The model

1. `select_until` sends `h` instead of `\x1b`: the same action, no ambiguous prefix, nothing to wait for. The comment says why. A scene that rebinds `back` away from `h` would need its own helper; the worker checks the 22 call sites' configs and reports that none does.
2. The reload scene's 50 ms sleep (`5557-5558`) becomes the same `h`, or a wait on a visible effect if that `Esc` closes something; the worker reads it and says which.
3. A rule in `docs/dev/tui.md`'s PTY-harness section: a bare `Esc` is sent only when the scene then waits for the visible effect it causes; where `Esc` only means `back`, send `h`.

### Tests first (D)

- No new test can fail deterministically on the old helper (the race is timing); the evidence is the audit: the report lists every `send(b"\x1b")` left and the wait that follows each, and the 22 `select_until` sites' keymaps. `just test-e2e` green; the PTY count stated.

## Deliverable B: range selection in the left pane

### What exists (facts at `dd069a7`)

- One selection: `App.selection: Option<Selection>` with `Selection::{Root, Row(root, path bytes), Group}` (`app.rs:230-244`); by path bytes, never by index (`docs/dev/tui.md:24-36`). `nav_entries()` (`app.rs:1760-1780`) is the flattened order, recomputed on every call; `move_selection` (`4194-4210`) walks it; `select()` (`4048-4092`) is the one setter and resets `diff`, `sel`, `press_line`, `drag_moved`; `reconcile_selection` (`4099-4126`) falls back row → root → nothing when a pile drops the row. No multi-row state exists (`nav_anchor` is scroll bookkeeping, `4048-4092`; `Sel` is the right pane's text selection, `1191-1201`).
- A click: `render_nav` (`render.rs:932-1129`) pushes `(rect, Target::NavRow(root, path))` per visible row; `Ui::event` (`run.rs:353-399`) turns a left press into `Action::Press(x, y)`, resolves it through `hits.at` and calls `App::hit` (`app.rs:5037-5143`; `NavRow` at 5117-5119). A press in a pass already marked changed is held and replayed against the next frame (`run.rs:577-590`, `tui.md:119-123`).
- Modifiers: `mouse_action` (`input.rs:1021-1030`) reads only `m.kind`; `m.modifiers` is never read. `Key::of` reads `SHIFT` for keys (`832-846`); `fold_shift` (`730-739`) makes `shift-j` the char `J`. Free defaults: `shift-j`, `shift-k`, `shift-up`, `shift-down`, `shift-v` (`DEFAULT_KEYMAP`, `470-542`).
- A group accepts as one: `accept_file_scope()` on `Selection::Group` yields `AcceptScope::Group` (`app.rs:2255-2264`); `accept_requests` (`2349-2414`, the Group arm 2373-2394) builds one `AcceptRequest::Group { rows, rendered_on }`; `Engine::accept_with` dispatches to `Ops::accept_group(rows, rendered_on, fault)` (`engine.rs:1718-1720`; `ops.rs:908-944`), which stages every row and writes one `UndoOp::AcceptGroup` entry, so `z` puts the set back. `accept_group` takes any `&[Rendered]` of one root; nothing in it requires a git-annotated group. `a` on a group refuses with "`A` accepts the group" (`2218-2221`). `counts_of` (`2417-2457`) feeds the confirm threshold `CONFIRM_ABOVE` = 10 (`2585-2602`; §6.7 line 426) and the hint.
- Completion wording in `App::accepted` (`3812`; Group 3898-3900, Root 3901-3910: `accepted N files in <root>`). The advance rule (§6.7 line 429) selects the entry that takes an accepted row's place.
- Flag, restore, expand are per row (`flag_target` 2637-2682; `restore_scope` 2282-2315; `request_expand` 1891-1930); the engine has no group restore.
- Rendering: the selected row is the whole row `REVERSED` (`render.rs:1113-1122`); no second nav style exists. Snapshots: `tui_nav_three_roots` (`tests/test_e2e_tui_snapshots.rs:229-256`) and its `_styles` twin show the shape.
- Terminals: inside a herdr pane every mouse event reaches the program (`tui.md:1202-1210`, "no bypass modifier"); outside herdr, shift plus the mouse is the terminal's own selection in every terminal targeted, and that is how the nav is copied today. So the mouse half of this deliverable works inside herdr and in any terminal that forwards a shifted press, and the keyboard half works everywhere. The docs say so plainly.
- Adding an action: `Action` (`input.rs:34-182`), `DEFAULT_KEYMAP` (its order is the help overlay's), `from_name` (569-614), `describe` (617-692, 30 columns), the `App::handle` arm, the reachability table `input_every_action_is_reachable` (1926-2062, asserts 55), `HINT_DROP_ORDER` and `hints()` (`render.rs:560-599`, 789-859), `docs/config.md` action table (223-263), `tui.md` key table (1096-1136), help-overlay snapshots and `help_two_column_width`.
- The PTY encoder has press, release and drag, no shifted press (`pty_tui.rs:781-810`); SGR 1006 spells a shifted left press as button 4 (`\x1b[<4;col;rowM`), which crossterm decodes into `KeyModifiers::SHIFT`.

### The model

1. **State.** `App.range: Option<NavRange>`, `NavRange { root: PathBuf, anchor: Vec<u8> }`: the anchor is a row path, never an index. The range is the contiguous run of `Selection::Row` entries of `nav_entries()` between the anchor and the current selection, both rows of the same root; a helper `range_rows()` walks the entries and stops at any `Root` or `Group` entry. `select()` clears the range except on the extend path; `reconcile_selection` drops the range when either end no longer resolves (a member vanishing in the middle shrinks it, since it is defined, not stored). `back` clears it.
2. **Actions.** `extend_down` (`shift-j`, `shift-down`) and `extend_up` (`shift-k`, `shift-up`): with the nav focused and a file row selected, set the anchor if none and move the selection one entry, keeping the range; refuse (no change, `Changed::No`) when the neighbour is not a file row of the same root. With the diff focused they do what `nav_down`/`nav_up` do not: nothing (`Changed::No`). `Action::ShiftPress(x, y)` from a left press whose modifiers contain `SHIFT` (`mouse_action`): resolved through the hit map; on a `NavRow` of the selected row's root with a file row selected it is `extend_to(row)` (anchor set if none, selection moved, range kept); any other target behaves as a plain press. Held and replayed exactly as `Press` is.
3. **Accept.** `AcceptScope::Rows { root, paths }`. `accept_file_scope()` yields it while a range is live; `accept_scope()` (the hunk key) refuses with "`A` accepts the N selected files" (the key spelled from the keymap as the group refusal does). `accept_requests` builds one `AcceptRequest::Group { rows, rendered_on: pile.seen_branch }` over the range's rows in nav order; `counts_of` tallies the same rows, so the confirm threshold and the hint agree by construction. No engine change. Completion: `accepted N files in <root>`; the advance rule selects the entry taking the range's place; the range is cleared.
4. **Everything else per row.** `a`, `m`, `shift-m`, `u`, `shift-u`, `e`, `enter`, `i` act on the selection (the cursor end) only.
5. **Render.** `NavLine.in_range`; a member that is not the cursor row is drawn `REVERSED | DIM`, the cursor row plain `REVERSED`, so the `_styles` snapshot tells them apart. The right pane shows the cursor row's diff, unchanged; the header's `[A accept file]` label follows `accept_file_scope()` as it does for a group (`A accept N files`).
6. **Hint line.** While a range is live the accept phrase reads `A accept N files` (N from `counts_of`), in the accept slot, offered only when N > 0. No new drop-order entry.
7. **Docs.** `docs/config.md` action table (two rows), `docs/review-loop.md` (one paragraph: a run of rows, the keys, shift-click inside herdr), `docs/dev/tui.md` (the state, the reducer arms, the hit-map rule for `ShiftPress`, the key table, the mouse paragraph rewritten so it says which half works where), `CHANGELOG.md` `## Unreleased` `### Added`, README's one-line key list if it names `shift-a`.

### Tests first (B)

- `app.rs`: `shift-j` twice from `f1` gives the range `{f1, f2, parse.rs}` and `range_rows()` in nav order; extending onto a root row, a group row or across a root refuses with `Changed::No`; a plain `j`, a plain click and `back` clear it; a pile that drops the anchor clears it and one that drops a middle member shrinks it; `a` on a range refuses with the file key named; `A` on a range builds exactly one `AcceptRequest::Group` with those rows; a range over `CONFIRM_ABOVE` opens the confirm with the count; after the accept the range is empty and the selection is the entry that took its place.
- `input.rs`/`run.rs` parity: a `MouseEvent { Down(Left), modifiers: SHIFT }` at `target_center(NavRow)` equals the `shift-j` run (the `mouse()` helper at `run.rs:2491-2496` gains a modifiers argument); a shifted press on a non-nav target is a plain press; a shifted press in a changed pass is held like `Press`.
- Reachability table: `ShiftPress` as `mouse`, the two extend actions as `key`; the count 55 becomes 58.
- Snapshots: `tui_nav_range_selection` (`_frame` and `_styles`) after two `ExtendDown` on the three-roots scene; the help overlay frames re-accepted for the two new rows, each diff read and listed in the report.
- PTY: `sgr_shift_press`/`shift_click` in `pty_tui.rs` with a unit test beside the encoders; the scene `pty_range_select_accepts_three_and_undoes_them`: click `f1`, shift-click `parse.rs`, `A`, `accepted 3 files in alpha`, the rows gone, `z`, the rows back; a second scene or the same one, keyboard only: `j`, `shift-j`, `shift-j`, `A`.

## Deliverable A: one pull request per release

### What exists (facts at `dd069a7`)

- `scripts/release.py` has three verbs. `prep <version>` (lines 182-269) insists on `main`, clean, at `origin/main`, a version later than `Cargo.toml`'s, no local tag, no `release/v<new>` branch; it dates `## Unreleased` (`208-227`), substitutes the version in `README.md` and `docs/install.md` (final versions only, exactly two hits each, `229-242`), sets `Cargo.toml`, runs `cargo update --workspace --offline` and requires `Cargo.lock` to move by exactly 3/3 (`253-258`), creates `release/v<new>` and commits `chore(release): <new>` (`244-262`). `merge [n]` (`272-318`) and `tag` (`326-401`) refuse in an agent's shell (`by_hand`, 90-95: `CLAUDECODE` in the environment); `tag` takes the version from `Cargo.toml`, refuses if the tag is on GitHub, refuses any `## Unreleased` on `main` (`353-354`), needs the ci run on the commit green (`358-366`), tags, pushes, watches `release.yml`. `RELEASE_DRY_RUN=1` prints instead of writing (`27-28`, 42, 77-82).
- `release.yml` needs only: the tag equals `v<crate version>` (`102-112`), a `--locked` lockfile (152), a non-empty `## <version>` section (`194-218`). It does not care which pull request set them. Branch protection needs the five contexts and an up-to-date branch (`docs/dev/publishing.md:58-67`); `ci.yml` runs on the merge commit (`push: branches: [main]`, lines 8-9), which is what `tag` waits for.
- `merge` already works for any pull request; `tag` already works from any `main` commit whose crate version has no tag (precedent: `v0.4.0` was tagged on the fix PR's merge, §10 2026-09-17).
- Every release since `v0.4.0` took three pull requests (the phase, `release/v<x>`, the post-release docs); the third exists because the §8 gate line "`vX` released by the sponsor's tag" and the amendments' RATIFIED marks were written after the tag with the tag object and run id (`00-spec.md`, the Phase 13 and 14 closing lines; `operations.md:140`).
- The convention lives in: `release.py:1-31` (the docstring), `justfile:250-271`, `AGENTS.md:31` and 133-134, `docs/dev/operations.md:24-54` and 86-100, the kickoff line "sits under `## Unreleased` until the release bump PR dates it" (`100-phase10-kickoff.md:5`), §10 2026-09-17.

### The model

1. **`prep` moves to the branch.** `just release-prep <version>` runs on any branch but `main`, clean tree. Guards, in order: a valid version; not on `main`; clean; the version later than `Cargo.toml`'s and later than the newest `v*` tag on `origin` (after `git fetch --tags`); the tag absent locally and on `origin`; `## Unreleased` present and non-empty (or an rc's dated `## <base>` as today); the install pages naming the shown version exactly twice each (final versions). It edits the same five files the same way and commits `chore(release): <version>` on the current branch. No `release/v<version>` branch. The undo line names `git reset --hard HEAD~1` only when the commit was made.
2. **`merge` and `tag` unchanged**, except `merge`'s "next: just release-tag" line prints when the merged PR's head commit changed `Cargo.toml`'s version (read from the diff), not from the branch name.
3. **The record is written before the tag.** The §8 gate line of a phase says "`v<version>` released by the tag on this PR's merge commit"; the amendments carried by the PR say "ratified by the merge of this PR" with no SHA; the tag object and the release run id, if wanted, go into the next phase's kickoff entry baseline. `operations.md`'s evidence table (line 140) is updated in the next phase's PR. No post-release pull request.
4. **The self-test.** `python3 scripts/release.py self-test` covers the pure helpers (version parsing and ordering, the CHANGELOG dating, the install-page substitution, the tag-and-version guards over a fake `ls-remote` output) with no git and no network; `just lint` runs it, so CI covers it on both runners.
5. **Docs.** `operations.md` release recipe (24-54) rewritten to: `just release-prep <version>` on the phase branch, push, one pull request, `just merge`, `just release-tag`; the invariants (86-100) kept; the "three pull requests" history in one sentence. `justfile` comment block and recipes; `AGENTS.md` golden path line and the operations stub; `release.py` docstring; the kickoff convention sentence in this file (below, under Docs).
6. **§10 entry** "2026-09-27 (the release flow)" with the sponsor's words and the superseded 2026-09-17 ruling named.

### Tests first (A)

- `self-test` red on the unchanged script for: `prep` refusing `main` (a fake branch name) and accepting a feature branch; the tag guard against a fake remote listing that already has `v0.7.0`; the `## Unreleased` dating; the two-hits rule. Then the implementation.
- `RELEASE_DRY_RUN=1 just release-prep 0.7.0` on the branch prints the plan and writes nothing (`git status` clean after); its output in the report.
- The real `just release-prep 0.7.0` is the orchestrator's, after the close-out docs commit, as the branch's last commit.

## Docs (all deliverables)

`docs/config.md`, `docs/review-loop.md`, `docs/dev/tui.md`, `docs/dev/engine.md`, `docs/dev/operations.md`, `docs/dev/testing.md` (the `Esc` rule if the harness section lives there), `CHANGELOG.md` under `## Unreleased` (Added: range selection; Fixed: the fold; the release flow is not a user-facing change and gets no bullet). House style: no em-dashes in the user docs, README, CHANGELOG, `operations.md`, this file; `docs/dev/tui.md` keeps its em-dash line count (210) and `docs/dev/engine.md` its (84) unless a rewritten sentence drops one, which the report states. The kickoff convention from this phase on: the CHANGELOG entry sits under `## Unreleased` until `just release-prep` dates it on the phase branch.

## Spec amendments carried by the PR (Amendment v1.17, PROPOSED)

§6.3 line 395: the fold clause reads "folding the paths whose merge-base entry is seen state" in place of "folding the paths finished on the left branch". §6.3 operations table: a row "accept a chosen row set: accept file, iterated over the rows of one repository the user selected as a run, from the run's confirm-time snapshot; one undo entry" after "accept upstream group". `01-scenarios.md` D24: a PROPOSED paragraph as in C.3. Ratified by the merge of this PR.

## Gate (mirrors §8 "Phase 15")

- [ ] C: the six-operation history as a fixed test, red first, green after; the `Ops`-level twin; every listed refusal still a refusal; `lc.sh` changed in step; `just test-scenarios` and `just harness` green; the seen-oracle proptest at 64 cases green in prepush; `engine.md` and the spec's PROPOSED text in.
- [ ] D: no `select_until` sends `Esc`; every remaining bare `Esc` is followed by a wait on its visible effect; the audit table in the report; the rule in the harness docs.
- [ ] B: the reducer tests, the parity tests, the reachability count 58, the snapshot scene and the help frames re-accepted with the diffs listed, the two PTY scenes; the docs say where the mouse half works; `just tryout` gains no scenario (the keys are in the existing walks' reach).
- [ ] A: `self-test` in `just lint`; `RELEASE_DRY_RUN=1 just release-prep 0.7.0` clean on the branch; the recipe, justfile, AGENTS.md and docstring rewritten; the §10 entry; then the real `just release-prep 0.7.0` as the branch's last commit and `v0.7.0` released by the tag on this PR's merge commit.
- [ ] Standing: unit floor 928 grows (count and split in the report); integration, e2e, scenarios, prepush counts stated; the em-dash counts; no personal detail in the diff.
- [ ] **[sponsor]** the cold read of `## Which key leaves what out` (the entry item), and a hands-on run of the range: click, shift-click, `A`, `z` inside a herdr pane, and `j`, `shift-j`, `A` outside one; the sponsor's words in §10.
- [ ] Amendment v1.17 ratified by the merge; the §10 close-out entry with the judgment-call list.
