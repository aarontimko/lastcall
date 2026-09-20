# Phase 13 Kickoff Prompt (operational artifact, not design)

**Rulings: FROZEN 2026-09-20** (§10 2026-09-20, "visual word wrap in the diff pane; the index seed lock"). **Adversarial design review: PENDING.** No worker exists until the review has run against this file and its findings are folded here and recorded in §10. Amendment v1.14 is PROPOSED in `00-spec.md` and is ratified by the merge of this phase's PR.

**The framing (sponsor, 2026-09-20):** in the first days of `v0.4.0` the sponsor read prose and long code lines in the right pane and found them cut off at the pane's edge, with no way to see the rest short of opening the file elsewhere. The diff pane draws one line of the file on one row of the screen and clips whatever does not fit; it has no sideways scroll. A review tool that hides the end of a line is asking the reader to accept text they have not read, which is the thing the tool exists to prevent. The sponsor asked for visual word wrap, on by default. The second deliverable is older debt with a due date: the §11 entry "the private index and its marker can disagree" names a way for a change to be hidden and sets its trigger at "no later than the phase after `v0.4.0`". That is this phase.

---

## Mission

Build Phase 13 per `00-spec.md` §8 "Phase 13", §6.1 (the `[ui]` table, Amendment v1.14), §6.7 (the main view wraps), the §10 entry of 2026-09-20 and the §11 entry on the index and its marker. Two deliverables, independent of each other, each with its own tests and its own commits:

- **A. Visual word wrap in the diff pane** (crate `lastcall`, `tui/`).
- **B. The index seed lock** (crate `lastcall-engine`, `index.rs`, `scan.rs`, `ops.rs`).

Reference implementations: `crates/lastcall/src/tui/render.rs` (`render_hunks`, `hunk_line`, `HitMap`), `tui/app.rs` (`DiffCursor`, `scroll_by`, `move_sel_cursor`, `move_hunk`, the page keys), `tui/run.rs` (`diff_line_at`, the hit write-back), `tui/textbuf.rs` (`wrap_ranges`, `cell_width`), `tui/input.rs` (`DEFAULT_KEYMAP`), `crates/lastcall-engine/src/config/mod.rs` (`HerdrConfig` as the table pattern), `index.rs` (`ensure`, `seed`, `write_marker`), `ledger.rs` (`LedgerLock`).

## The sponsor's rulings (verbatim in §10; summarised here)

1. **The scroll position stays a line of the diff, not a row of the screen.** `DiffCursor::scroll` keeps its meaning (an index into header + lines + separator per hunk). The renderer wraps. Nothing that stores or compares a scroll position changes meaning.
2. **One wrapped line never fills the pane.** A line that would wrap to more rows than the cap is cut at the cap and its last row ends in `…`. The cap is a few rows short of the pane's body height, so the line after it is always at least partly on screen.
3. **Wrap is on by default, and it has a toggle.** `alt-z` is bound (the key VS Code users know). A second, plain-character binding exists because Option/Alt does not reach every terminal (`docs/config.md` already explains this for `alt-up`). The sponsor ruled the plain key is **not** `shift-w`: "it's not semantically close to what 'w' does". A config default exists: `[ui] wrap = true`. Both keys are rebindable under `[keys]`.
4. **Break at a word boundary; hard-break a word longer than the row; the `+` / `-` gutter and the colour repeat on every continuation row.**
5. **The index seed lock goes into this phase** as a second, separate deliverable with its own tests.

**Orchestrator taste-call, labelled as one, open to the design review:** the plain key is **`c`** (clip or wrap; free today, as is `x`), and the action is named **`wrap`**. The hint-line label is `c wrap`. `alt-z` is listed first in `DEFAULT_KEYMAP` so the help overlay shows both; the hint line shows `c` because it works everywhere.

## Entry baseline (inherited obligations)

- **Unit-test floor: 781** (`just test-unit` on main `b41a72a`: 780 at `0f32531` plus the index-marker test of PR #35). The suite only grows. Other tiers at the close of Phase 12: integration 131, harness 217 assertions, prepush 150, PTY 49 + 1 ignored, snapshots 62 + 1 ignored, the real-herdr subset green in CI, `just audit` in CI only. **Re-measure the floor as your first cargo action and report the number you saw.**
- **Entry obligation from §11** (deliverable B): the index and marker hide, trigger "the next engine change, and no later than the phase after `v0.4.0`".
- On record, not this phase: the real-size responsiveness run on a second machine (§11), the agent candidate for a watched folder (§11), the "N repos hidden" wording (§11), the shared temp names in three unserialised writers (§11, R2 and R4).

## Entry preconditions (orchestrator-confirmed before launch)

1. Toolchain via rustup (1.98.0, edition 2024); cargo only through `just …` / `just cargo …`. The pre-commit hook runs `just lint && just test-unit` (run `just cargo fmt --all` first). The pre-push hook runs `just test-prepush`; you never push.
2. You build on the phase branch **`feat/phase13-wrap`** in the main checkout (every `git` is `git -C` that path, every `just` is `just -d … -f …/justfile`). **First action:** `git -C … status --porcelain` empty and `git -C … log --oneline -1` showing this kickoff's freeze commit or a later orchestrator commit. Stage files by name only. Never commit on `main`, never push, never tag, never stash, never `git add -A` or `git add .`.
3. **You never install anything.** No `cargo install`, no `brew`, no `rustup component add`, no new tool of any kind. `cargo-deny` is absent on this machine and `just audit` runs in CI only; say so in the report instead of installing it. A new crate dependency is not expected by this phase: if you believe one is needed, stop and report.
4. Verified facts you may rely on (the working tree at `b41a72a`; line numbers checked by the orchestrator, re-check them before you cite them):
   - **One line is one row today.** `render_hunks` (`tui/render.rs:1678`) computes `total = diff_lines(hunks)`, `offsets = hunk_offsets(hunks)`, clamps `app.diff.scroll`, finds the hunk with `partition_point`, and draws each line with `buf.set_line(area.x, area.y + y, &line, area.width)`, which clips at the pane's width. It is called twice: `:1626` (`HunkControls::FlagOnly`, the expansion of a collapsed row) and `:1652` (`HunkControls::All`).
   - `hunk_line` (`render.rs:1866`) builds the line: the header through `hunk_header`, a context line as `" {text}"`, an insert as `"+{text}"` green, a delete as `"-{text}"` red. `line_text` (`:1885`) strips `\n` / `\r` and turns a tab into four spaces, so the text the wrapper sees has no tabs.
   - The header row carries more than text: `Target::DiffHunk(h)`, the right-aligned `[a accept] [u restore] [m flag]` controls (`right_align_run`), the flag-note marker (`marker_budget`, `flag_marker`), and the `band` (`:1798`) for the current hunk. The selection (`app.sel.map(|s| s.range())`) is a reversed band per line.
   - `HitMap` (`render.rs` about `:156`) has `nav_top`, `editor`, `diff_body: Option<Rect>`. `run.rs:393` `diff_line_at` is `rect.contains(..).then(|| self.app.diff.scroll + (y - rect.y))`: row offset equals line offset, which wrap breaks. Mouse press and drag use it (`run.rs:354`, `:365`); the wheel is `Action::ScrollUp` / `ScrollDown` (`:324`, `:369`). The renderer's geometry already flows back to the `App` once per frame (`hits.nav_top` → `app.nav_top`, `run.rs:446`).
   - `DiffCursor { hunk, scroll }` (`tui/app.rs:296`). `page_rows()` (`:1718`) is `size.1 - 4`, an approximation: the pane also draws a path line and sometimes a banner, so the body is shorter than that. `editor_cols()` (`:2607`) is how the `App` derives the pane's width from `size` and `nav_width`. `scroll_by` (about `:3773`), `move_sel_cursor` (`:3795`, keeps the selection's end on screen with `page_rows`), `move_hunk` (`:3816`, scroll = the hunk's offset), the page keys (`:3981` to `:4008`), `hunk_offsets` (`:4613`), `diff_lines` (`:4624`).
   - `tui/textbuf.rs:762` `wrap_ranges(text, width) -> Vec<(usize, usize)>` hard-wraps by display width through `cell_width` (`:118`), always returns at least one range, and is private. The note modal uses `Wrap::Soft` (`render.rs:2245`); the inline editor uses `Wrap::None` (`app.rs:2622`) and **stays unwrapped in this phase**.
   - `DEFAULT_KEYMAP` (`tui/input.rs:456`) is the `[keys]` table and the help overlay's order. `c` and `x` are unbound. `alt-` parses (`input.rs` about `:711`). The comment above `nav_top` records why a plain twin exists for every Alt binding. The hint line (`render.rs:588` `hints`) is full at 100 columns and drops hints one at a time from the right (`render_hints_drop_one_at_a_time_from_the_right`, `:3531`).
   - `Config` (`config/mod.rs:64`) is `deny_unknown_fields` with `default`; `HerdrConfig` (`:187`) is the table pattern with a manual `Default`; `validate` is at `:408`. There is no `[ui]` table today. Settings the TUI writes go through `config/write.rs`; **the toggle is session-only and writes nothing**.
   - `PrivateIndex::ensure` (`index.rs:118`) reads the marker and reseeds on a mismatch; `seed` (`:129`) removes the marker, runs `read-tree`, writes the marker through `write_marker` (`:140`). Callers: `scan.rs:350` (`ensure`), `scan.rs:361` (`seed`, the unreadable-index retry), `ops.rs:2492` (the fold, four lines after `drop(_lock)` at `:2486`). `LedgerLock` (`ledger.rs:817`) is the house lock: `std::fs::File::try_lock`, bounded retry 40 × 50 ms, released on drop, no `libc`, no `unsafe`. `RepoPaths` (`paths.rs:100`) names every file in the repo state directory; `is_temp_index_name` (`:83`) is what the sweep may delete.

## Deliverable A: visual word wrap

### The model

- **One pure layout function, used by everyone.** A new module (suggested `tui/wrap.rs`) holds (a) `wrap_words(text, width) -> Vec<(usize, usize)>`: char ranges, break after the last whitespace that fits, hard-break a run longer than the row, measured with the same `cell_width` the note editor uses (make it `pub(super)`; do not copy it), always at least one range, never an empty range except for an empty line; and (b) the pane layout: given the hunks, the scroll line, the body's columns and rows, and the wrap flag, the list of screen rows, each naming its line index and which part of that line it is. The renderer draws from that list, the hit map is that list, and the `App`'s keep-visible arithmetic asks the same function. **No second implementation of "which line is on which row" may exist.**
- **Width.** The text wraps at `body columns - 1` (the gutter column). A continuation row repeats the gutter character (` `, `+`, `-`) and the colour. Leading indentation is not repeated on continuation rows (taste-call: it costs the columns a narrow pane does not have). The hunk header row does not wrap: it clips as today, and its controls, marker, target and band are untouched.
- **The cap.** `cap = max(1, body_rows - 3)`. A line that wraps to more than `cap` rows draws `cap` rows and its last drawn row ends in `…` in place of its final cell. With wrap off the cap does not apply.
- **The scroll.** `diff.scroll` is still a line index and the top row of the body is always the first row of that line. The wheel and `j` / `k` move by lines; a step may therefore move several rows. Accepted by ruling 1.
- **Geometry the `App` needs.** The renderer writes the body's size back (`hits.diff_body` already carries the rect; `run.rs` copies columns and rows into the `App` beside `nav_top`). Before the first frame the `App` falls back to `editor_cols()` and `page_rows()`. `move_sel_cursor` keeps the selection's end fully on screen using the layout function. The page keys move by the number of lines the layout put fully on screen (at least one). `move_hunk` is unchanged.
- **Hit testing.** `HitMap` gains the row table (`diff_rows: Vec<usize>`, one line index per drawn body row). `diff_line_at` looks the row up there; a row below the last drawn row answers `None` as it does today. Every row of a wrapped line selects that line.
- **Bands.** The selection band and the current-hunk band cover every row of a line they cover.
- **The toggle.** `App` gains `wrap: bool`, initial value `config.ui.wrap`. Action `wrap`, default bindings `["alt-z", "c"]`. Toggling keeps `diff.scroll` and redraws. The hint is `c wrap` (or the user's rebinding through `hint_label`), placed so it is among the first hints dropped when the line is full; it shows only where the diff pane has hunks. The help overlay lists it on the second page with the other view toggles, described as `wrap long lines`.
- **Config.** `[ui]` table, `UiConfig { wrap: bool }`, default `true`, `deny_unknown_fields`. `wrap = "yes"` and `[ui] wrapp = true` are load errors with the usual message shape.

### Tests first

- Unit, `wrap_words`: the parts concatenate to the input; no part is wider than the row; a break falls after whitespace when one fits; a 300-column word hard-breaks; CJK and emoji widths; width 1 and width 0 (returns the whole text in one range, never loops); an empty line is one empty range. A proptest for the first two properties (8 cases in the unit tier, 64 in prepush, the house split).
- Unit, layout: a scroll at line L puts L's first row at row 0; the cap and the `…`; wrap off reproduces today's one row per line exactly; the row table has one entry per drawn row; a body of 1, 2 and 3 rows does not panic and draws something.
- Unit, reducer: `move_sel_cursor` over a line that wraps to five rows keeps the end visible; page down then page up returns to the same scroll when no line is capped; the toggle keeps the scroll; the config default reaches `App::wrap`.
- Unit, renderer: a `+` line's continuation rows start with `+` and are green; the selection band is on every row; the header row still carries its controls with a wrapped line below it; a click on a continuation row selects the line (through `diff_line_at`).
- Snapshots (`crates/lastcall/tests/test_e2e_tui_snapshots.rs`): a long-line scene at 100×30 and at 80×24 with wrap on; the same scene after the toggle; a capped line; `[ui] wrap = false` at launch. **Regenerate existing frames in one commit of their own**, and list in that commit's message which frames changed and why (the expectation: only frames with a line wider than the pane, plus any frame whose hint line gained `c wrap`). A frame that changes for another reason is a finding, not a regeneration.
- PTY: `c` toggles; `ESC z` (what a terminal sending Option as Meta emits for `alt-z`) toggles; neither waits on time.
- Config: the two load errors; `[ui]` absent equals `wrap = true`; `[keys] wrap = ["x"]` rebinds both the key and the hint.

## Deliverable B: the index seed lock

### The defect (from §11, PLAUSIBLE, not reproduced)

`seed` is three steps with nothing held across them, and the fold seeds after it drops the ledger lock. Process A, still on tree X, and process B, folding to tree Y, can interleave as: B removes the marker, B `read-tree Y`, A removes the marker, A `read-tree X`, A writes marker X, B writes marker Y. The index now holds X under a marker that says Y, every later `ensure` for Y calls it fresh, and a file accepted into Y and then put back to its X content is not listed. That is a hide, and it persists until something else reseeds.

### The fix

- A new lock file `index.seed.lock` beside `index` (`RepoPaths::index_seed_lock`), taken the way `LedgerLock` is taken (`File::try_lock`, bounded retry, drop releases; no new dependency, no `libc`, no `unsafe`). The sweep must never delete it: pin `!is_temp_index_name("index.seed.lock")`.
- **Held exclusively across the whole of `seed`** (marker removal, `read-tree`, marker write). With that alone the index and the marker can never be observed in lasting disagreement: the last seeder wins both.
- **Held across the scan's read as well:** from `ensure`'s marker read through `refresh` and `diff_files` (and the retry seed at `scan.rs:361`). Without it a transient remains: A reads marker Y as fresh, C (stale on X) seeds X, A's `diff-files` runs against X while A believes Y, and the put-back file is missing from that one scan. One exclusive lock for both uses is the simple shape; scans of one root by two processes are rare and the hold is one `diff-files` long. `seed` must not try to take a lock its caller already holds (an inner `seed_locked`, or a guard passed in).
- **Lock order, stated in the code and tested:** the ledger lock is never requested while the seed lock is held. The fold takes and drops the ledger lock first, then seeds under the seed lock (as it orders them today). Confirm by reading every path from `scan` that can reach `LedgerLock::acquire`; if one exists, report it before building.
- **When the lock is busy past the retry budget:** the scan fails with a typed error the engine already knows how to show (the last pile stays on screen and a notice names the lock), as a busy ledger lock does. It never proceeds unlocked, because proceeding unlocked is the hide. The fold's seed, which runs after the ledger is already committed, on a busy lock leaves the marker **removed** and returns the error: a missing marker makes the next `ensure` reseed, which is the safe direction.

### Tests first

- A deterministic interleaving test, red before the fix: drive two `PrivateIndex` values over one state directory through the six-step order above using a test seam (a hook between `read-tree` and the marker write, test-only, `cfg(test)` or a fault point in the house style), and assert that afterwards index and marker agree or the marker is absent. Show it red on `main`'s `seed` in the report.
- The engine-level statement of the same thing: two engines on one root, one accepts while the other scans on the old tree, then the file is put back; the file is listed by both. With the seam, not with sleeps.
- The busy paths: a held seed lock makes a scan return the typed error and leaves the pile as it was; a held seed lock during the fold's seed leaves no marker; the next scan reseeds and the pile is correct.
- Lock order: a test that holds the ledger lock while a scan runs to completion (proving the scan never waits on the ledger lock while holding the seed lock), and the reverse for the fold.
- `index_marker_writes_at_the_same_moment_both_succeed` and `engine_two_engines_scan_one_root_concurrently_and_agree` still pass, 20 runs each in a loop, no flake.
- `docs/dev/engine.md`: the state-directory listing (about `:45`), the sharing table (`:189`), the sweep paragraph (`:199`) and the failure table (`:545`) gain the lock.

## Docs (both deliverables)

`docs/config.md`: the `[ui]` table, the `wrap` row in the keys table, one sentence beside the existing Option-key paragraph saying `alt-z` has the plain twin `c` for the same reason. `README.md` only if it describes the diff pane's clipping today (check; do not add a feature list). `CHANGELOG.md` `## Unreleased`: Added (wrap, the toggle, the config key), Fixed (the index and marker hide, worded for a user: two lastcall processes on one repository). `docs/dev/engine.md` as above. **No em-dashes in any of these files; no personal detail; the sponsor is "the sponsor".**

## Gate checklist (mirrors `00-spec.md` §8 "Phase 13")

- [ ] Wrap: one layout function shared by the renderer, the hit map and the reducer (named in the report, with the proof no second one exists); word break, hard break, gutter and colour on continuation rows; the cap with `…`; the scroll still a line index; selection, bands, clicks, page keys and the hunk jump correct over wrapped lines; wrap off reproduces the pre-phase frames byte for byte.
- [ ] Toggle and config: action `wrap` on `alt-z` and `c`, rebindable, in the help overlay and the hint line; `[ui] wrap` default `true`, unknown keys and wrong types refused; the toggle writes nothing to disk.
- [ ] Snapshots: new scenes (wrap on at two sizes, toggled, capped, config off); existing frames regenerated in one commit of their own with the changed frames listed and explained; no frame carries a temp path.
- [ ] Seed lock: the interleaving test red before and green after; index and marker never in lasting disagreement; the scan's read under the lock; the busy paths safe (never unlocked, never a stale marker); the lock order stated and tested; no new dependency; the sweep never takes the lock file.
- [ ] Docs: `docs/config.md`, `docs/dev/engine.md`, CHANGELOG `## Unreleased`; `just lint` green; the em-dash grep empty; no personal detail in the diff.
- [ ] Standing: the unit floor grows (count and split in the report); integration, harness, prepush, PTY and snapshot counts stated; the real-herdr subset green in CI; `just audit` in CI only.
- [ ] **[sponsor]** on the sponsor's own machine: long prose and long code lines read in full in the right pane; the toggle by both keys in the sponsor's terminal (or a note of which key the terminal delivers); selection and copy over a wrapped line; the sponsor's words in §10.
- [ ] Amendment v1.14 ratified by the merge; the §11 index and marker entry closed with the evidence; the §10 close-out entry with the judgment-call list; `v0.5.0` released by the sponsor's tag.

## Operational rules

- One cargo process at a time on this machine; you are the only one while you run. Never run `find ~`. Shell is zsh: absolute paths, `git -C`, quote globs, `sed -i ''`, no `timeout`, no foreground `sleep`, `grep` is ugrep, `rg` exists. `rm -rf` is blocked: use fresh timestamped directories.
- **Never install anything** (precondition 3).
- Never read or write `~/.local/state/lastcall`, `~/.config/lastcall`, `~/.config/herdr`, `~/.local/bin/lastcall`; every test and probe isolates `LASTCALL_STATE_DIR`, `HOME`, `XDG_CONFIG_HOME` and clears `HERDR_*` (the fixtures already do). Never override `HOME` for a cargo or just command. Do not touch `~/dev/git/herdr`, any other checkout, or anything outside the repository and the scratchpad.
- Conventional commits, one concern each, `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`; commit messages through a file and `git commit -F`; the hook runs lint and unit on each commit; `just test-prepush` before the report. Deliverable A and deliverable B never share a commit.
- Never end your turn to wait. When something in this kickoff is wrong against the code, say so in the report and take the smallest correct reading; when a rule here would hide a change the user has not seen, stop and report instead of building it.

## Working agreements (the verifier's hunts, so you can pre-empt them)

- V1 (hidden text): a character of a line that appears on no row with wrap on and no cap in play; a wide character split across the edge; the `…` replacing a character on a line that was not capped; a continuation row without its gutter or colour; a wrapped deletion read as context.
- V2 (the wrong line): a click, drag or `v` selection on a continuation row that lands on the next line; `y` copying rows instead of lines (a wrapped line must copy as one line, with no inserted newline and no `…`); a flag placed on the wrong line number.
- V3 (lost position): the toggle, a resize, a nav drag or a live re-render moving the scroll; page down skipping a line entirely; `move_sel_cursor` leaving the selection's end below the fold; a body of one or two rows panicking or looping.
- V4 (two layouts): any place that still assumes row = line (`scroll + (y - rect.y)`, `page_rows` arithmetic in the diff reducer, the expansion path at `render.rs:1626`); the renderer and the reducer disagreeing about the body's size on the first frame or after a resize.
- V5 (frames): an existing snapshot changing with no long line and no hint change; wrap off not byte-identical to before; the hint line at 100 columns losing a hint the loop needs.
- V6 (the lock): the six-step interleaving still reachable through the `scan.rs:361` retry or any `seed` caller that skips the lock; a deadlock between the seed lock and the ledger lock in either order; a busy lock falling through to an unlocked scan; a stale marker left after a failed seed; the lock file swept as a temp file; a test that passes because of a sleep.
- V7 (docs): `docs/config.md` naming a key the keymap does not have; the changelog promising sideways scroll or editor wrap, neither of which this phase builds.
