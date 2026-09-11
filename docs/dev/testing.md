# Testing

**The sacred rule, in one line: tests never touch the real herdr config or socket — every test
injects `Env`.** The only file in the engine that reads `std::env` is
`crates/lastcall-engine/src/env.rs` (`rg -n 'std::env::var|home_dir\(' crates/lastcall-engine/src`
must match only that file); unit tests build an `Env::empty(..)` and add exactly the variables
they mean to test. The real-herdr spawner removes every inherited `HERDR_*` variable from the
child and never falls back to a `herdr` on `PATH`.

## Tiers

| tier | command | what runs | where |
|---|---|---|---|
| unit | `just test-unit` = `cargo test --workspace --lib --bins` | in-module `#[cfg(test)]` only | everywhere, incl. macOS CI |
| integration | `just test-integration` = `cargo test --workspace --test 'test_integration_*'` | real git; the four-test real-herdr subset when `LASTCALL_TEST_HERDR_BIN` is set (`just test-integration-herdr` sets it from the pinned release; `just test-integration-herdr-latest` from herdr's newest) | CI via `just test-integration-herdr`; Linux blocking, macOS best-effort; the latest-release run is the weekly `herdr-compat` workflow, never a blocker |
| e2e | `just test-e2e` = `cargo test --workspace --test 'test_e2e_*'` | the TUI: fifty-two `TestBackend` snapshot scenes and twenty-four PTY scenes (plus one helper unit test in the same file) against the built binary (`docs/dev/tui.md`) | everywhere; the PTY file skips with a visible reason only where no pseudo-terminal can be opened |
| bench | `just bench` = release build, then `cargo test --release -p lastcall --test test_bench -- --ignored --nocapture --test-threads=1` | the four `#[ignore]`d baseline scenarios (`docs/dev/bench.md`); **not a gate** in Phase 4 — targets are set at the Phase 9 kickoff | by hand, on the machine named in `bench.md` |
| pre-push | `just test-prepush` = `just test-integration`, then `PROPTEST_CASES=64 cargo test -p lastcall-engine --lib proptests` | the integration tier plus the two store-backed proptests at 64 cases (the unit tier runs them at 8) | the pre-push hook; by hand before a push from a machine without the hook |

`just test` runs the three tiers in order. **`just test-unit` is the canonical suite**; its count is
the ratchet floor from Phase 2 on (Phase 1 close: 85 engine + 16 testkit + 0 binary = 101, the
Phase 2 floor; Phase 2 close: 166 engine + 16 testkit + 0 binary = 182, the Phase 3 floor; Phase 3 close: 168
engine + 19 testkit + 64 binary lib + 4 binary main = 255, the Phase 4 floor; Phase 4 engine
work (4a): 179 engine + 19 testkit + 64 binary lib + 4 binary main = 266; Phase 4 TUI work
(4b): 180 engine + 19 testkit + 95 binary lib + 4 binary main = 298; the rename-pairing fix:
183 engine = 301; the Phase 4 review fold: 183 engine + 19 testkit + 100 binary lib + 4
binary main = 306; Phase 4 e2e + bench work (4c): 184 engine + 21 testkit + 100 binary lib
+ 4 binary main = 309; the Phase 4 close-out review fold (the rss sampler tests): 184
engine + 23 testkit + 101 binary lib + 4 binary main = 312; the hunk
separator and the selected-header band: 102 binary lib = 313; the nav-pane hunk accept, the
focus arrows and the shift-drag note: 106 binary lib = 317; the deletion-row hunk accept
(sponsor-found, `a4e353c`): 185 engine = 318, the Phase 5 floor; Phase 5 engine and TUI work
(5a/5b): 205 engine + 24 testkit + 146 binary lib + 5 binary main = 380; Phase 5 real-server
and schema work (5c, the `herdr_schema` projection's own tests and the isolation-collision
test): 34 testkit = 390, the Phase 6 floor; Phase 6 engine work (draft roots, collapsed
classes, `hunks_of`, the debounce cap, one remote-ref listing): 215 engine + 34 testkit +
146 binary lib + 5 binary main = 400; Phase 6 TUI work (the expand key, the drain pass, the
nav offset, the discovering line, the debug probes): 218 engine + 34 testkit + 164 binary
lib + 6 binary main = **422**, the Phase 7 floor; Phase 7 (the engine half: restore, flags,
the export renderer and the ledger's 1.1 schema; then the TUI half: the two-column overlay,
restore, the note modal, the picker and the export fallback, whose reducer tests are the last
13 of the binary lib's count): 255 engine + 34 testkit + 187 binary lib + 6 binary main =
**482**; then the verifier (b) review-fix pass, which added seven reducer and render tests
for F1–F5: 255 engine + 34 testkit + 194 binary lib + 6 binary main = **489**; Phase 8 (save under CAS, the `$EDITOR` handover, the inline editor, select-to-copy, the launch hold): 271 engine + 35 testkit + 250 binary lib + 6 binary main = **562**, the Phase 9 floor; Phase 9a (the `status` store fields, every repo listed and `t`, the neighbour rule, the hint line's drop order, the focus-true opening, the launch hold's one vocabulary and scope fold, the editor header, the help overlay's clip row, and the verifier (a) folds): 274 engine + 35 testkit + 267 binary lib + 6 binary main = **582**, the Phase 9b floor; Phase 9b so far (herdr v0.9.0's protocol set, the `[update]` config key, the header's update notice, and `commands/update.rs`'s own tests, which land in the binary main count because `commands/` is binary-only): 277 engine + 35 testkit + 268 binary lib + 16 binary main = **596**).
The suite never shrinks across commits. One recorded exception: at the Phase 2 code review
the three filesystem-live watcher tests (up to 30 s waits, real FSEvents) left the unit tier
for `crates/lastcall-engine/tests/test_integration_watcher.rs` because they contradicted the
determinism rules below (162 → 159 engine), and the same review added six engine unit tests
(159 → 165). The pure routing/allowlist watcher tests stay in `watcher.rs`.

## Hooks (`just hooks-install`)

`core.hooksPath` is pointed at the committed `.githooks/`. **pre-commit** runs `just lint`
and `just test-unit` — exactly the gate's lint and unit commands, so every commit is green
under both and the unit tier must stay fast (a few seconds in the engine crate). **pre-push**
runs `just test-prepush`: the integration tier and the store-backed proptests at 64 cases,
the checks that are too slow for every commit but must be green before anything leaves the
machine. Both print what they are running and fail the commit or push on the first red step.
CI runs the same targets (`just lint`, `just test-unit` with `PROPTEST_CASES=64`,
`just test-integration-herdr`, `just test-e2e`).

## Scenario suites (`just test-scenarios`)

`crates/lastcall-engine/tests/test_integration_scenarios_{a..f}.rs` hold one
`scenario_<id>_<slug>` test per `docs/spec/01-scenarios.md` ID (A accepts, B history, C
upstream, D edge cases, E storage faults, F draft roots). Each builds a `FixtureRepo` (or a
parent dir of them) in a temp dir, opens the engine through `tests/common::Fresh` with first
sight done, mutates the worktree/history with real git, and asserts the pile in the
harness's format with `lastcall_testkit::assert_pile!` — the expected string of every
harness-covered (H) scenario is copied verbatim from `scripts/harness/scenarios.sh`. E1
re-executes the test binary as a child role and SIGKILLs it at each fault point
(`FaultPoint::AfterObjectWrite`, `AfterLedgerTmpWrite`); D4 (case-only rename) and D6 (sparse
checkout) self-skip with a printed reason when the filesystem or git cannot produce the
precondition. `just test-integration` runs them too.

`crates/lastcall-engine/tests/test_integration_accept_loop.rs` (Phase 4 kickoff deliverable
9(a)) drives the whole reviewer loop through `Engine::accept` — the TUI's path — against a
fixture "agent" that edits three files and commits behind the reviewer: hunk → file →
accept-all → drop the engine, agent commits the rest, reopen → one more edit shows exactly
that delta. Every step asserts the override map, then the folded seen tree (entries by oid)
and `seen_at.head_commit`, which the agent's commits never move.

## The `status --json` golden

`crates/lastcall/tests/test_integration_status_golden.rs` builds one temp parent dir with
`lastcall_testkit::fixture_parent` (repo A: an uncommitted edit plus an agent commit; repo B:
a fast-forward pull of two coworker files with one edited on top; a draft dir with one edit —
first sight of all three happens **before** those operations), runs the built binary
(`env!("CARGO_BIN_EXE_lastcall")`) with `LASTCALL_CONFIG` naming a config whose
`parent_dirs = [W]` and `LASTCALL_STATE_DIR` in a temp dir, replaces `W` with `<W>`, and
compares byte-for-byte to `crates/lastcall/tests/golden/status_multi_repo.json`. Commit oids
are stable because fixtures use fixed identities and dates. To update after an intentional
schema change: `just golden-update` (sets `LASTCALL_UPDATE_GOLDEN=1`), then review the diff
and commit the file.

## The flag-export goldens

Two files under `crates/lastcall/tests/golden/` freeze `flags::export`'s bytes, and one
`just` target rewrites both:

```sh
just flag-export-golden      # LASTCALL_UPDATE_GOLDEN=1, then a plain run to prove it passes
```

- `flag_export.md` is written by the engine unit test
  `flags::tests::flags_export_matches_the_golden` under a `FixedClock`, and covers the
  awkward shapes on purpose: a hunk flag, a file flag with no diff block, a note full of
  control bytes in caret form, and a hunk whose own body contains a three-backtick fence, so
  that block has to open with four. Phase 8 added the **whole-file** case in both its forms
  (Amendment v1.8's additive `Flag.summary`): a file flag that carries the counts —
  `· whole file` on the header line and `3 hunks · +12 −4` on its own line under it — and,
  right after it, a Phase-7-shaped file flag with **no** summary, so the golden freezes that
  an old ledger entry still renders and that a collapsed row nobody expanded prints no
  counts line at all (verifier (a) F2). Both are in the one file on purpose: the two shapes
  sit adjacent in the diff, so a change that drops the summary line cannot pass as a change
  that never wrote one.
- `flag_export_pty.md` is written by the PTY scene
  `pty_flag_note_exports_when_standalone`, so it is the export as the **built binary**
  appends it to the fallback file — the whole path, note modal included. Two fields a run
  can move are normalised before the compare: the root basename to `<R>` and the timestamp
  to `<T>` (the binary has no clock override, which is why the engine golden and not this
  one pins a real timestamp).

Regenerate only through the `just` target, review the diff, and commit the file with the
change that moved it.

## The e2e tier: snapshots and the PTY

Both files live in `crates/lastcall/tests/`; `docs/dev/tui.md` has the how-to.

`test_e2e_tui_snapshots.rs` renders fifty-two scenes (the sixteen Phase 3 ones; the
seven Phase 4 accept scenes, which drive the real `Engine::accept` from the reducer's own
`Effect::Accept` and feed `App::accepted`, and where `tui_accept_all_confirm` pins a second
`_live` frame; and the six Phase 5 herdr scenes — `tui_herdr_status_dots`,
`tui_herdr_ready_ack_dims`, `tui_herdr_flag_only_root_listed`, `tui_herdr_header_states`,
`tui_herdr_scope_notice`, `tui_herdr_scope_notice_with_status`, fed by a `HerdrView` built
in-process, with no socket anywhere; `tui_herdr_header_states` pins one snapshot rather than
two, being a list of badge lines and not a frame; and the four Phase 6 ones —
`tui_draft_root_hunks`, whose fixture adds the opt-in fourth root `W/alpha/_drafts` through
`fixture_parent::add_draft_root` and so is the **only** scene that is not three roots,
`tui_nav_collapsed_lockfile`, `tui_nav_collapsed_binary_and_size` and
`tui_diff_view_collapsed_expanded`, which presses `e` and pins the expansion under the
collapsed line; and the six Phase 7 ones — `tui_note_modal`, `tui_agent_picker`,
`tui_restore_confirm`, `tui_diff_view_flagged_hunk`, `tui_nav_flag_counts` and
`tui_help_overlay_tall`, where the flagging scenes drive `m`, the note a character at a
time and Enter, then call the real `Engine::flag` with the effect's own arguments and feed
`App::flagged` back, so the frame is of an `App` the loop could have produced; and the ten
Phase 8 ones — `tui_note_modal_scrolled` and `tui_note_modal_whole_file` for the modal's
text area and its target title, `tui_editor_return_confirm` for the `$EDITOR` blessing,
`tui_editor_open`, `tui_editor_dirty_confirm`, `tui_editor_save_refused` and
`tui_editor_narrow_60x20` for the inline editor, and `tui_diff_selection`, `tui_copy_cue`
and `tui_hint_diff_focus` — the last snapshotted at **142×20**, the only scene wide enough
for the whole diff-focused hint line; and the five Phase 9a ones — `tui_nav_empty_repo_row` and
`tui_hide_empty_toggle` (three roots, one with nothing pending, before and after `t`),
`tui_accept_last_file_lands_on_the_repo_row` (renamed from `tui_accept_last_file_collapses_repo`,
because the frame changed meaning under Amendment v1.9), `tui_editor_long_path_60x20` (the
head-ellipsized editor header) and `tui_help_overlay_80x24` (the clipped overlay's `… N more keys`
row)) through `ratatui::backend::TestBackend`
from an `App` fed by a real engine over the shared `fixture_parent` (each scene builds its
own fixture and state dir under a temp dir) and pins each as two `insta` snapshots under
`crates/lastcall/tests/snapshots/`: `<scene>_frame` (the symbols, exactly as a 100×30 — or
the scene's own size — terminal would show them) and `<scene>_styles` (the non-default style
runs: `<row> <from>..<to> <fg> <bg> <modifiers>`, which is where an inverted hunk header or a
focused border is visible). A failing snapshot test prints insta's unified diff: `-` lines
are the committed frame, `+` lines the new one; a moved column or a changed count is a
real change, a temp path is a leak (frames must show basenames and root-relative paths
only). To accept an intentional change: `just snapshots-update`, read every rewritten
`.snap` in the diff, commit them with the code change. Never regenerate to make red go
green.

`test_e2e_tui_pty.rs` spawns the built binary (`env!("CARGO_BIN_EXE_lastcall")`) with `tui
--poll 1` inside a real pseudo-terminal (`lastcall_testkit::pty_tui`; `portable-pty` +
`vt100`) over a fresh fixture parent, with `HOME`, `LASTCALL_CONFIG` and
`LASTCALL_STATE_DIR` injected per scene, and asserts on the parsed screen: first frame,
live update after an edit (the clock starts after the write returns; the minimum of two
tries must be ≤ 1.75 s and both are printed), hunk keys, a mouse click, a resize, and the
restored terminal after `q` / Ctrl-C (the raw transcript must carry the mouse-off and
alternate-screen-off sequences and no log line). The Phase 4 scenes
(`pty_accept_loop_and_restart`, `pty_accept_refused_when_file_moves`) drive `a` / `A` /
`ctrl-a` + `y` against a fixture agent's edits, read the `ledger.json` files back after the
fold, and relaunch a **second process** on the same state dir to show the empty state. The
three Phase 5 scenes point the built binary's `HERDR_SOCKET_PATH` at a
`lastcall_testkit::mock_herdr` socket rather than a real herdr:
`pty_herdr_flag_ack_jump` (the version in the header, the working dot, a `done` pushed as
`pane.agent_status_changed` growing a bright flag, `d` dimming it — bold is the one attribute
vt100 keeps — `g` sending `agent.focus` with the public pane id, and a clean `q` with the
link live), `pty_herdr_a_stalled_socket_does_not_hold_the_keys` (a socket that accepts and
never answers: `q` still exits inside the quit budget while the 5 s guard timeout runs), and
`pty_herdr_worktree_created_reaches_the_nav_through_the_loop` (a checkout made after startup,
with the discovery backstop parked at `--poll 300`, so only the loop's `worktree_due` arm can
bring it in). The Phase 6 scene `pty_draft_root_hunk_accept_and_restart` runs the binary
over `draft_config_toml`'s four roots — it must, or the child would discover only three —
and accepts a hunk in the gitignored `_drafts/` root, then relaunches on the same state dir
to show it stayed accepted; `wait_first_piles` additionally pins the startup order
(`lastcall: discovering roots under …` on stderr, then the alternate-screen sequence, then
the hold's own pane text `discovered N repos, checking status…` — Phase 9a deliverable 5 took
`scanning N roots…` off the status line, and `wait_first_piles` pins that it stays off; it also returns only once the engine's `watching …` notice is on the status row, because on a CI runner that notice lands seconds after the first piles and a scene that pressed a key in the gap saw it cover the hint line or the key's verdict — PR #9's first CI run). The four Phase 7 scenes are `pty_restore_hunk_then_file_bytes_match_baseline` (restore one
hunk, then the file, comparing the bytes on disk with the baseline blob),
`pty_restore_refused_when_the_file_moved` (the file is renamed under the running loop and
the refusal is read off the status line), `pty_restore_deletion_recreates_the_file`, and
`pty_flag_note_exports_when_standalone`, which types a note into the modal with **no herdr
link** and reads the export back out of the state dir against `flag_export_pty.md`. The note
it types is four lines and gets there both ways a note can: a raw `0x0a` (what the terminal
sends for `Ctrl-J`) and a bracketed paste carrying a newline of its own, both through the
real crossterm reader — so the failure `tui.md` calls the worst this modal has, firing off
the first line and dropping the rest, is now proven absent end to end and not only in the
reducer (verifier (b) F6). The Phase 9a scene
`pty_accept_last_file_lands_on_the_repo_row_then_t_hides_it` accepts a repo's last row through
the terminal and asserts the cursor on that repo's own name row with `nothing pending in` on the
pane, then `t` hiding the repo and `t` bringing it back; it is the slow one — the bottom row is
the status line while a status is live, and launch sets one, so the scene waits out
`app::STATUS_TTL` (30 s) once before it can read the hint line (Phase 9a verifier (a) F8: about
36 s of the file's time is that wait). The three scenes for the update check are
`pty_update_notice_after_hold` (a served `latest.json` naming a newer release: the raw
transcript proves the notice never precedes the launch hold, the header then carries the
seven-column `↑ 9.9.9`, a click reads the whole sentence onto the status line, and after a
clean exit the probe log holds exactly one `releases/latest` URL against `api.github.com`,
never the test base URL), `pty_update_check_is_throttled_by_the_daily_stamp` (a stamp 1 h old
renders the notice from disk and spends no request; one 25 h old looks again and moves
`checked_at` forward), and `pty_update_check_is_off_for_every_other_scene` (the default
isolation: no notice, no request, no stamp, and `[update]` / `check = false` in the config the
harness wrote). Every negative assertion in the three is made **after** `wait_exit`, so no
detached check thread can race it. No PTY
scene talks to herdr: only `just test-integration-herdr` proves the real pane. The staged
send is covered in three places, and it takes all three — verifier (b) F1 found that the two
end tests both passed while the middle was missing, because nothing in the loop built
`HerdrUpdate::Agents` and the send was unreachable in the binary. The reducer's decision
(`app_flagged_with_one_agent_stages_without_asking` and the picker tests) injects the
candidates; `herdr_stage_wraps_the_export_in_bracketed_paste_markers` pins the request's
shape against an in-memory mock; and **`loop_flag_with_one_agent_reaches_pane_send_text`
(`tests/test_integration_loop_flag_stage.rs`) is the only test that joins them** — a real
client over the mock socket, the loop's own `run::herdr_fold`, `Ui::event` for the
keystrokes, and `run::spawn_flag` / `run::spawn_stage` for the effects, asserting the
bracketed-paste `pane.send_text` that lands on the socket. A send test that injects its own
candidates proves the reducer, never the wiring. The scenes are serialized (one
mutex); the whole file is about 105 s. Timing lines go to `stderr().write_all` so they survive libtest's
capture — run it with `-- --nocapture` to see them. If the live-update assertion fails on a
loaded host, report the measured numbers; do not loosen the budget.

### The probe editor (`tests/probe/editor.sh`)

`shift-i` spawns whatever `$VISUAL`/`$EDITOR` names. **No test may reach a real editor.** One
would take the terminal the harness is driving and wait for a human, and the developer's own
`$EDITOR` is not the harness's to run — so `PtyCommand::isolated_lastcall` **removes** both
variables from every child, and each editor scene sets `EDITOR` to an **absolute path** inside
its own temp dir. `PATH` is never touched, globally or otherwise.

The program at that path is `crates/lastcall/tests/probe/editor.sh`, reached through a
**symlink the scene creates named `vim`**, so `EditorCommand`'s basename table gives it the
`+<line> <file>` argv shape and the scene can assert the line lastcall chose. One script
serves every scene, driven entirely by the environment:

| variable | what the "editor" does |
|---|---|
| `LASTCALL_PROBE_EDITOR_LOG` | append `argv: …` and `cwd: …` — this is how a scene proves the line flag and that the child's cwd is the root |
| `LASTCALL_PROBE_EDITOR_SLEEP` | sleep first, so the scene can type at the terminal while the "editor" owns it (the `^C` scene) |
| `LASTCALL_PROBE_EDITOR_WRITE` | rewrite the file named by the **last** argument with this content — the save a real editor would have made. Unset means look and quit, which must leave the file alone |

`set -u`, and deliberately **no** `set -e`: a scene that interrupts the sleep with `^C`
expects the script to die from the signal. The exit status is never asserted, because
lastcall treats an editor that exits non-zero exactly like one that exits 0 — either way the
only question is what the file holds now.

`test_integration_editor.rs::editor_launch_lands_at_the_right_line` is the smallest scene
that uses it — one `shift-i` on `alpha/src/parse.rs`'s second hunk, asserting the log's
`argv:` is `+<line> <absolute path>` and its `cwd:` is the root — and it lives in the
integration tier because it is the argv proof, not the terminal-handover proof; the PTY
scenes are the latter.

### The probe curl (`tests/probe/curl.sh`)

`lastcall update` and the TUI's daily check reach the network by spawning `curl`
(`commands/update.rs`, rule 1: no HTTP crate). **No test may reach the network.** Two
independent guards stop it. First, `isolated_lastcall` writes `[update]` with `check = false`
into every isolated config, so a scene that never thought about releases starts no check at
all; `.update_check(true)` is the opt-in the three update scenes use. Second, it prepends a
probe directory to the child's `PATH` whose `curl` is a symlink to
`crates/lastcall/tests/probe/curl.sh`, so even a bug that started a check unasked would reach
the script and not the internet. (`PATH` is prepended only for the child, and only here; the
editor scenes still never touch it.)

The script serves the directory named by `LASTCALL_TEST_RELEASE_DIR` and **exits 99 if that
variable is unset**, which is what makes an accidental request loud instead of silent. It maps
a request path to a file: `*/releases/latest` to `latest.json`, `*/releases?per_page=*` to
`list.json`, and `*/download/<tag>/<asset>` to `<asset>`. A `<name>.status` file beside it
sets the HTTP status (the rate-limit scene), a `<name>.headers` file the response headers
(`-D -`). It honours `-o <dest>`, appends the three status digits to stdout the way
`-w '%{http_code}'` does, and logs `url: <url>` to `LASTCALL_PROBE_CURL_LOG`, which is how a
scene proves how many requests were spent and against which host.

The integration tier uses the same script directly
(`crates/lastcall/tests/test_integration_update.rs`, eight scenes: the verified replacement, a
checksum mismatch that leaves the binary alone, `--check`, the prerelease that is not offered,
the package-manager refusal, the rate limit and its reset time, the loopback-only
`LASTCALL_UPDATE_BASE_URL`, and the unset-directory failure). Those scenes copy the built
binary into a temp dir first: nothing ever renames over the binary the test runner is using.

### The slow git (`scripts/slowgit/git`)

Not a test fixture but a viewing aid, kept beside the probe editor here because it is the
same idea — a stand-in program driven by the environment. The engine spawns `git` through
`PATH` (`git::base_command`), so a `git` placed first on `PATH` that sleeps and then execs
the real one stretches the scans without touching the code or the build. It sleeps only
before `diff-files` and `ls-files`, the two subcommands the scan runs and discovery does
not (the split was measured by logging every call of a launch: discovery is `rev-parse`,
`config`, `symbolic-ref`, `cat-file --batch-check`, `ls-tree`), so the
`lastcall: discovering roots…` line and the first frame arrive as fast as ever and only
the launch hold — `discovered N roots, checking status…`, then the counter and the ✓s —
is prolonged. `just probe-tui-slow` runs `probe-tui` under it with `alpha` as the slow
root; `SLOWGIT_MS`, `SLOWGIT_SLOW_REPO`, `SLOWGIT_SLOW_MS` tune it. Use it whenever a
change touches what the screen shows *while* it is waiting — the fast path hides all of
that — and when checking a UI on hardware slower than the machine it was built on.
**No test uses it**: the harness never touches `PATH`, and a scene that needs a slow scan
gets it from fixture size, not from a sleeping `git`.

### `LASTCALL_KEYBOARD=plain` in the harness

`isolated_lastcall` also sets `LASTCALL_KEYBOARD=plain`. The harness answers no terminal
query, so an unskipped keyboard-enhancement probe would cost **every** scene crossterm's full
2 s timeout at startup (ruling P9; `bench.md` "Known costs"). Exactly one scene removes the
variable — `pty_keyboard_enhancement_probe_is_answered_and_swallowed` — and plays a
kitty-protocol terminal, proving the query is written, the answer is believed, the flags are
pushed and popped, and no byte of the reply ever reaches the app as a key. Any new scene that
wants the probe must remove the variable itself and pay for it.

### `PtyCommand::answer_cursor_position` (off by default)

The harness is a terminal that answers **nothing** — that is the point of
`LASTCALL_KEYBOARD=plain` above — with one opt-in exception. `answer_cursor_position()` makes
the reader thread reply to every `ESC [ 6 n` (DSR, "where is the cursor?") in the child's
output with `ESC [ 1 ; 1 R`, the way a real terminal would. The row and column are invented:
nothing reads the report back, because crossterm swallows it as an internal event.

It exists for one behaviour. After an `$EDITOR` suspend, `Suspend::run` writes that query to
wake a tty whose already-readable byte kqueue never reported (verifier (b) F1; the mechanism
is in `tui.md`'s suspend step list). Only `pty_editor_key_typed_during_the_editor_is_not_stuck`
turns it on — take the switch out and that scene hangs on an unanswered confirm, which is
exactly the residual on a terminal that does not answer. Every other scene leaves it off:
a reply is bytes in the child's input that nothing else expects, written from the reader
thread the moment the query is seen, and it would change what the next `raw()` assertion or
`wait_for` sees. `pty_tui_answers_a_cursor_position_request_only_when_asked` (testkit unit
tier) covers both settings with a shell child that reads the reply back out.

## Naming

- Unit tests live in-module (`#[cfg(test)] mod tests`) and nowhere else: a `tests/test_unit_*.rs`
  file would not be run by `--lib --bins` and would not be counted.
- Integration tests: `crates/<crate>/tests/test_integration_<topic>.rs`.
- End-to-end tests: `crates/<crate>/tests/test_e2e_<topic>.rs`.
- Cargo errors on a `--test` glob with zero matches, so each tier keeps at least one file.

## Determinism rules for the unit tier

- No network, no git repos except temp fixtures, no sockets except the in-test mock.
- The herdr client state machine is tested on the **in-memory** mock transport under
  `#[tokio::test(start_paused = true)]` with production timings (500 ms coalesce, 30 s
  fallback, 250 ms → 10 s backoff): a paused clock costs nothing. Never pause time over a real
  socket — the clock auto-advances whenever the runtime idles on the read and every timer
  fires "instantly".
- Transport tests use the socket mock with real time and timeouts ≤ 50 ms.
- No sleeps longer than 50 ms anywhere in the unit tier. Anything that waits on a real
  filesystem watch or a polling backstop is an integration test.
- Property tests over real git (`ops::tests::proptests`, Phase 4 kickoff deliverable 8:
  accept-all-then-edits and hunk/file accept interleavings) run through
  `proptest::test_runner::TestRunner` so one draft-root fixture per test is shared across
  cases, with `failure_persistence: None` (no `proptest-regressions/` files to commit).
  The case count is `crate::env::proptest_cases()`: **8 by default** (the unit tier, so
  the pre-commit hook stays fast) and `PROPTEST_CASES` when set — the pre-push hook and
  CI export 64. It is read explicitly (and in `env.rs`, the engine's one `std::env`
  reader) because an explicit `cases:` field in a `ProptestConfig` silently overrides the
  variable proptest would otherwise honour. Each case reuses the fixture's store and
  rewrites its file set; at ~13 ms per git spawn on the development machine the two tests
  take ~2.5 s and ~3.5 s at 8 cases (they were ~9 s and ~14 s at the original 32, the two
  slowest unit tests by far; the pair is ~28 s at 64). The pure `hunks` proptest stays at
  1000 cases.
- A test that guards against a hang (`engine_scan_returns_under_a_global_fsmonitor_config`)
  runs the engine on a thread and bounds it with `recv_timeout` (5 s) — the bound is a
  failure, never a wait the passing path takes.
- **Nothing calls `tracing::subscriber::set_global_default`**, in any tier. `tracing` caches
  a callsite's `Interest` process-wide — the first thread to reach it decides for every
  other — so a `with_default` scope in a lib test can be silently disabled by a sibling test
  that reached the same callsite on a subscriber-free thread. The probe-field test
  (`crates/lastcall-engine/tests/test_integration_tracing.rs`, Phase 6 deliverable 8) is
  therefore its **own integration binary**: one process, one subscriber scope, no siblings
  to race. Any future test that captures `tracing` output belongs in that binary.

## The real-herdr subset (`test_integration_herdr_real.rs`)

Four tests in `crates/lastcall-engine/tests/`, run against a **real** herdr the test spawns
itself. `just test-integration-herdr` fetches the pinned release and exports
`LASTCALL_TEST_HERDR_BIN`; without that variable every one of them prints the sanctioned
`SKIP:` line and returns. That is the **only** skip they are allowed: any other reason a
test cannot do its job — no socket, a protocol the guard refuses, a spawn that never came
up — is a failure, because a subset that quietly turns into no subset is worse than no
subset at all.

**The spawned herdr stays off the network**, which takes saying because two of its own
defaults are on: `version_check` and `manifest_check` both default to true, and a server
started with them asks `herdr.dev` for the latest version and for the agent-detection
catalogue in the background, twice per spawn. `HERDR_TEST_CONFIG` writes both as `false`
under `[update]`, which is where herdr keeps them (written flat at the top level they are
unknown keys and herdr ignores them without a word), beside `onboarding = false`, and the
spawn environment points
`HERDR_AGENT_DETECTION_MANIFEST_CATALOG_URL` at a closed loopback port as well, so a build
that ever ignored the config line fails to connect rather than leaving the machine. To check
it, put a logging shim named `curl` first on `PATH` and run the subset: the log stays empty.

| test | what it proves |
|---|---|
| `herdr_real_ping_bootstrap_events_and_done_derivation` | the ping, the guard, the bootstrap snapshot, live events, and §5.7's `done` derivation from a real server |
| `herdr_real_done_flip_heals_within_fallback` (G3) | the silent `done → idle` flip on focus, which herdr announces with **no** event, is healed by the periodic fallback resync |
| `herdr_real_disconnect_reconnect_converges` (G6) | the cache converges after the server is really stopped and started again |
| `herdr_real_schema_consumed_surface_unchanged` | the API surface we consume is byte-identical to the pinned fixture |

**The pin is `herdr_version := "v0.9.0"`** (`justfile`), whose asset answers `ping` with
**protocol 22**. The guard accepts `[20, 21, 22]`: 20 is what the v0.8.2 asset answers, 21 is
the protocol §5 was hand-checked against (herdr master `5158ada`), 22 is the pin. When the pin
moves, the three behavioural tests are re-run by hand against the **previous** asset as well,
so the widened guard is proven on both wires rather than asserted:

```sh
LASTCALL_TEST_HERDR_BIN=target/herdr/v0.8.2/herdr \
  cargo test -p lastcall-engine --test test_integration_herdr_real -- --test-threads=1
```

`herdr_real_schema_consumed_surface_unchanged` is **not** part of that second run and cannot
be: it compares the binary under test against the fixture generated from the *pinned* asset,
so any other asset is a diff by construction. That is the point of the test, and its printed
diff is the evidence — at the v0.8.2 -> v0.9.0 move it listed exactly the four additive fields
in both directions (`WorktreeListParams.trust_repository`,
`ServerCapabilities.{endpoint_protocol_generation, health_check, surface_interest}`) plus the
protocol number, and nothing lastcall reads.

**G3 is the one that is easy to make vacuous**, and two things keep it honest. The client is
wrapped in a `FilteringTransport<T: Transport>` that drops the events the flip might
otherwise be announced through and re-serves the rest — `EventStream::new` takes a
`Box<dyn AsyncRead>`, so the filtered lines go back through a `tokio::io::duplex` and the
client cannot tell. And the test *settles* first: it waits for 800 ms of quiet on the push
stream before focusing the tab, because herdr announces a new tab's pane asynchronously long
after `tab.create` returned, and any lifecycle event schedules a 200 ms coalesced resync that
would heal the flip for the wrong reason. After the focus it asserts that the heal arrived
within `fallback + 1 s` (3 s + 1 s in this test; the interval timer is mid-period when the
tab is focused, so a run heals in about 2 s) and came through a full `Resync(Snapshot)`, that
no reconnect happened, and that herdr forwarded **nothing** in between: if it ever does, the
filter list is incomplete or herdr found another way to announce the flip, and the assertion
message says to report it rather than to widen the filter. A third guard asserts the filter
forwarded *something* overall, so a pipe that swallowed every line could not pass as a clean
run; the scene creates one extra tab after connecting to satisfy it, because until v0.9.0 it
was satisfied by herdr's lifecycle replay burst on connect and that burst is now fixed.

G6 stops the server through `SpawnedHerdr::stop_server` (`herdr server stop` over the
isolated socket, never a signal and never the user's herdr), waits for the process to go, and
brings it back with `SpawnedHerdr::respawn(&HerdrIsolation)` on the same socket path. The
cache is proven to have re-bootstrapped by `Cache.resyncs` going `1 -> 2`, not by a timer.

## The consumed API surface, and the weekly compat check

`herdr api schema --json` is 255 KB of JSON-Schema, 91 request and 58 result variants, almost
none of it ours — diffing all of it would flag every unrelated herdr feature, and an alert
nobody trusts is not a check. `lastcall_testkit::herdr_schema` projects it onto the surface
this repo actually consumes: the eleven methods we call with their params and result schemas,
the fifteen §5.4 lifecycle events plus `pane.agent_status_changed`, the transitive type
closure, and the `AgentStatus` and `NotificationShowSound` vocabularies the code branches on.
About 50 KB, key-sorted (`serde_json`'s maps are `BTreeMap`s here — no `preserve_order`), one
trailing newline, so a regeneration is byte-stable.

- `$ref`s are re-keyed `<section>/<Name>` rather than merged: herdr defines `PaneInfo`,
  `AgentStatus` and `TabInfo` separately per section, and merging them would hide the day one
  of them changes alone. `AgentStatus` is the single case where cross-section sameness is
  asserted, because our code assumes it.
- The method → result mapping is not derivable (herdr's `ResponseResult` is one flat
  `oneOf`), so `CONSUMED_METHODS` states it and the generator verifies each named result
  const exists in the schema.
- `just herdr-schema-fixture` regenerates
  `crates/lastcall-testkit/fixtures/herdr/schema/consumed-surface.json` from the **pinned
  release asset** — never master, never by hand. `herdr api schema --json` needs no server,
  so neither the generator nor the test touches a socket.
- `herdr_real_schema_consumed_surface_unchanged` compares and prints a path-by-path diff on
  mismatch, and asserts the projection is not vacuous (a method count, a result const per
  method, an event count equal to `wire::LIFECYCLE_SUBSCRIPTIONS.len()`, both pinned enums,
  more than twenty defs) so an empty projection can never pass.

`just herdr-fetch-latest` downloads herdr's **newest** release into `target/herdr/<tag>/` and
`just test-integration-herdr-latest` runs the subset against it.
`.github/workflows/herdr-compat.yml` does that on a schedule — Mondays 06:00 UTC, plus
`workflow_dispatch`, plus `pull_request` on its own paths so it has a green run before a
change to it merges. A failure there is **information, not a blocker**: `ci.yml` is untouched
and keeps using the pinned tag. The job files exactly one issue, labelled `herdr-compat`,
creating the label with `--force` if it does not exist yet and refusing to file while any
open `herdr-compat` issue exists (with a same-title search as a second guard), because drift
persists until someone adopts the release and a second issue would only be the same news
again.

To adopt a new herdr: bump `herdr_version` in the `justfile`, run `just herdr-schema-fixture`,
read the fixture diff and update `consumed-surface.json.provenance.md`, then
`just test-integration-herdr`.

## Not a tier: the install smoke (`just install-smoke`)

`scripts/install-smoke.sh` is the proxy for the one thing no tier can cover: a machine that
has never seen this project installing a published binary. Every tier runs against the built
tree with the toolchain already on the box, so nothing else would notice an asset that was
never uploaded, a `SHA256SUMS` whose names do not match the assets, a binary that needs a
glibc the target does not have, or an update path that works only where it was compiled.

Each leg starts an empty `ubuntu:24.04` container, installs `curl` and `ca-certificates`,
downloads the asset and `SHA256SUMS`, verifies the checksum, runs `--version`, then serves a
newer release from `python3 -m http.server` **inside** the container (the update path's test
base URL is loopback-only, which is why the server has to be in there) and drives
`lastcall update --check` and `lastcall update` against it. The layout it serves is the one
`commands/update.rs` asks for: `repos/<owner>/<repo>/releases/latest` for the API answer and
`<owner>/<repo>/releases/download/<tag>/<asset>` for the bytes, the same shape as the probe
`curl` above. The updated binary's digest is compared with the served asset's, so the run
proves the replacement really is what was downloaded and verified.

Both `linux/amd64` and `linux/arm64` run by default, the first under emulation on an
Apple-silicon host: without it, x86_64 Linux would ship untested. `--from-dir <dir>` takes
the assets from disk instead of the network, which is how the pipeline is exercised before
any release exists. With no second release named, the smoke serves the installed binary back
under the next patch version, so the update path is still driven end to end; naming a second
real release additionally proves `--version` changes. The version it serves back is
`next_version`'s: the next patch for a release, and for a release candidate the release it is
a candidate for, since `0.1.0-rc.1` sorts below `0.1.0`. That is the one piece of arithmetic
in the script that could be wrong quietly, so `scripts/install-smoke.sh --self-test` checks it
against a table of cases and exits, with no Docker and nothing downloaded. No Docker, or no
daemon: the recipe says so and exits 2. It is a recipe, never a test: nothing in any tier
reaches the network.

## Not a gate: `test_perf_scan`

`crates/lastcall-engine/tests/test_perf_scan.rs` is `#[ignore]`d evidence, not a tier: 2,000
tracked files, an unreadable ledger (so every file is a row), and `PERF` lines with the
process-wide git spawn count (`lastcall_engine::git::spawn_count`) and wall time per scan,
then accept-all and a no-change scan that must hash nothing. Run it by hand with
`cargo test -p lastcall-engine --test test_perf_scan -- --ignored --nocapture` when touching
the scan pipeline; at the Phase 2 close a scan of everything-unseen was 16 git processes and
under 0.5 s (it was 2,018 processes and 25 s before blobs were fetched in one
`cat-file --batch`).

## Not a gate: `test_bench` (`just bench`)

`crates/lastcall/tests/test_bench.rs` is the Phase 4 performance baseline: four
`#[ignore]`d scenarios at the sizes ruled in `docs/spec/93-phase4-kickoff.md`, run against
the release build only (a debug build prints a SKIP line) and recorded in
`docs/dev/bench.md`. Each prints `BENCH <scenario> <metric>=<value>` lines; fixture
construction is outside every timed region; RSS is `ps -o rss=` sampled by the PTY harness
(`PtyCommand::sample_rss`, `PtyTui::peak_rss_kb`). No target is asserted in this phase
(S4's `rows_shown == cap` and `omitted == 50,000 − cap` are correctness assertions, not
budgets); the Phase 9 kickoff sets the targets against these numbers.

## How skips are reported

The real-herdr tests need the pinned binary, and that variable being unset or empty is the
**only** skip they may take — everything else is a failure (see the subset section above). Without
`LASTCALL_TEST_HERDR_BIN` each writes
`SKIP: LASTCALL_TEST_HERDR_BIN unset (run: just test-integration-herdr)` with
`stderr().write_all` (libtest swallows `eprintln!` of passing tests) and returns; `just
test-integration` prints the same skip at the shell level, so it is visible twice rather than
never. `just test-integration-herdr` fetches the pinned release (`gh release download`, the only
sanctioned network fetch besides cargo and rustup) and exports the variable.

## Isolation for the real herdr (`lastcall_testkit::herdr_spawn`)

Per spawn: `/tmp/lc-<pid>-<nanos>-<n>/` (never `$TMPDIR` — macOS caps Unix socket paths at 104
bytes; the path is asserted under 100). The trailing counter is load-bearing: macOS's
`SystemTime::now()` is microsecond-grained, so two tests in one binary that spawn together
used to land on the same base and the second herdr exited with `error: herdr server is
already running`. The base is created with `create_dir`, so any future collision is a plain
error rather than a shared socket, and the drain thread keeps the last 8 KiB the server wrote
so a start that fails quotes herdr's own words. Inside that base: private
`XDG_CONFIG_HOME`, `XDG_RUNTIME_DIR`, `HOME`, `XDG_STATE_HOME`, `XDG_DATA_HOME`,
`XDG_CACHE_HOME`, an explicit `HERDR_SOCKET_PATH`, `SHELL=/bin/sh`, `HERDR_TEST_CONFIG`
written before spawning (`onboarding = false`, then `version_check = false` and
`manifest_check = false` under `[update]`),
`HERDR_AGENT_DETECTION_MANIFEST_CATALOG_URL` pointed at a closed loopback port, and every
inherited `HERDR_*` removed. Socket readiness is polled (`exists && connect`) every 25 ms
up to 5 s. Kill-on-drop and kill-on-panic through a PID registry with a matcher
(`ps -o comm= -p`) that refuses to kill anything it did not spawn. Safe wrappers only
(`unsafe_code = "forbid"`, no `libc`).

`stop_server` asks the server to quit over **its own** isolated socket (`herdr server stop`,
never a signal, never the user's herdr) and waits for the pid to leave `ps`; `respawn` starts
a new one on the same socket path with the same `HerdrIsolation`, which is what G6 needs. One
subtlety is load-bearing there: `portable_pty` gives the child its own session with the pty as
its **controlling** terminal, and the kernel's revoke at exit blocks until the tty output
queue drains — so `spawn_server_child` runs a thread that reads the master (keeping only the
tail).
Without it a stopped herdr wedges in macOS `ps` state `E` and `stop_server` times out after
10 s; with it the stop takes about 200 ms. The timeout error prints a `ps` line for the pid,
so a future wedge says what state it wedged in.

## Fixtures

`crates/lastcall-testkit/fixtures/herdr/`: each fixture has a `<name>.provenance.md` saying
whether it was recorded from the pinned binary (preferred) or hand-written from the schema,
and from which schema lines. `just herdr-record` re-records into `recorded/`;
`just fixtures-sync` derives the named fixtures. `fixtures/herdr/schema/consumed-surface.json`
is the odd one out — generated by `just herdr-schema-fixture` from the pinned release's own
`api schema --json`, never edited by hand; its provenance file records the tag, the exact
command, the date and the generator's summary line. Fixture git repositories come from
`lastcall_testkit::fixture_repo::FixtureRepo` (deterministic identity, dates, and config; a
local bare `origin`; `coworker_push`), proven by `test_integration_fixture_repo.rs`.

**`alpha/src/parse.rs` (Phase 8 deliverable 10; sponsor direction 2026-09-02).** The three-root
parent's `f1`/`f2`/`f3` are one-line files, which is enough to prove a pile and not enough to
edit: an editor scene needs a file with a real middle, and a hunk whose first changed line is
neither line 1 nor a leading-context line. `fixture_parent::build` writes a 54-line config
parser — doc comment, struct, two functions, a `mod tests` — and **commits it before the
first sight**, then has "the agent" edit it in three separated places (the doc comment, a
condition in the middle, a new test at the bottom) without committing, so the row has three
hunks with real context between them. The addition happens in `build`, after
`FixtureRepo::new_in`, so **only alpha** gets it, no existing file's content changes, and
every `f1`/`f2`/`f3` assertion still holds (design review F7). `PARSE_RS_EDIT2` and
`parse_rs_edit2_line()` are exported beside it: the second hunk's text and its line number,
computed from the fixture text **independently of `Hunk::editor_line()`**, so a test that
asserts where an editor opened is not asserting the implementation against itself (F5, F9).
`lastcall_testkit::engine` opens an engine over one (`open_engine`, `assert_pile!`, the
`KillAt` fault injector); `lastcall_testkit::fixture_parent` builds the golden's three-root
parent dir (also the `just probe-status` / `probe-watch` fixture via
`examples/fixture_parent.rs`). Every engine in a test gets its `Env` from
`FixtureRepo::engine_env` / `engine_env_for` (private `HOME`, null global git config,
`LASTCALL_STATE_DIR` in a temp dir): tests never touch `~/.config` or `~/.local/state`.
