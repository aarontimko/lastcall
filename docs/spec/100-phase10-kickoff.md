# Phase 10 Kickoff Prompt (operational artifact, not design)

**Rulings: PROPOSED 2026-09-13; frozen by the orchestrator after the adversarial design review, under the sponsor's standing delegation (§10 2026-09-05).** The sponsor's direction, verbatim (2026-09-13): "1 rec, 2 rec, 3 rec -- -let's also loop in those minor doc changes into this new branch --- proceed with the normal phased-program as a mini spec with adversarail review, etc before dispatching". The three items are his own feedback from using `v0.1.0` (§10 2026-09-13 entry, where his words are recorded): a first-launch flow that greets a herdr user and explains the two behaviours a new user meets first; an undo for an acceptance made too fast; a way to snooze a repository that is being worked in but should not clutter the list. A fourth item, a bug he has still to describe, is reserved as deliverable 4 and is added by a dated orchestrator edit when it arrives. Changes to this file after the freeze are orchestrator-only and dated.

The program's Gate 9 closed on 2026-09-12 (`v0.1.0` at `8c9fb93`; main `88dd310`). This phase is the first one after the release: it is built on a branch from main, ships as one PR, and its CHANGELOG entry sits under `## Unreleased` until the release bump PR dates it (`docs/dev/operations.md`, the release sequence). Unit floor **604** (lastcall lib 270, bin 20, engine 277, testkit 37); every tier green on merged main.

## Mission

Build the three features the sponsor ruled on, against the frozen contracts as amended by **Amendment v1.11** (proposed here, ratified by the sponsor merging this phase's PR): §6.2's two additive ledger fields (`undo`, `snoozed_until`), §6.1's first-launch marker and the one sanctioned config write, §6.3's three new operations, §6.7's overlay and keys, and `status --json`'s two additive per-root fields. §5 unchanged; `01-scenarios.md` unchanged. Read, in this order:

- `00-spec.md` §1.5, §2, §3.4, §6.1, §6.2, §6.3, §6.7, §8 Phase 10, §10 from "2026-09-12" to the end, §11, Amendments v1.9 to v1.11.
- `docs/dev/tui.md` ("Architecture", "Startup and the first frame", "The accept loop", "Restore and flag" for the note modal, "Keys", "The `[keys]` table", "herdr in the UI", "Hit-testing", "Adding a widget, with a snapshot", "The PTY harness", "Gate greps"); `docs/dev/engine.md` ("Storage walkthrough", "Accepting through the engine", "Restore and flag", "`status --json` schema"); `docs/dev/testing.md` ("Tiers", "The e2e tier", "Naming").
- `crates/lastcall-engine/src/{ledger.rs, ops.rs, engine.rs, status.rs, config/mod.rs, paths.rs}`; `crates/lastcall/src/{commands/config.rs, commands/update.rs, commands/status.rs, tui/app.rs, tui/render.rs, tui/run.rs, tui/herdr.rs, tui/input.rs}`; `crates/lastcall-testkit/src/pty_tui.rs`; `crates/lastcall/tests/{test_e2e_tui_pty.rs, test_e2e_tui_snapshots.rs, test_integration_status_golden.rs}`; `justfile`.
- `docs/config.md`, `docs/review-loop.md`, `docs/herdr.md`, `README.md` (the user docs this phase extends; their house style is the rule: no em-dashes, no process vocabulary, no email addresses, no home paths).

## Entry baseline (inherited obligations)

- **Unit-test floor: 604** (`just test-unit`). The suite only grows; a shrinking count at any commit is a verifier failure class. Other tiers at close of Gate 9: e2e 36 PTY scenes + 54 snapshot scenes (one pre-existing ignore each), integration green across every binary, the real-herdr subset on the `v0.9.0` pin, prepush proptests, the status golden, the flag-export goldens.
- **The branch already carries** `17ff752` (`docs/dev/operations.md` house-style sentence, `docs/dev/publishing.md` §6 denylist grep), the sponsor's "loop in those minor doc changes". Nothing to do; do not revert it.
- **No entry obligations** were handed to this phase by an amended gate.

## Entry preconditions (orchestrator-confirmed before launch)

1. Toolchain via rustup (`just toolchain`, 1.98.0 with clippy and rustfmt); cargo only through `just …` / `just cargo …`. The pre-commit hook runs fmt, clippy `-D warnings`, `check --no-default-features` and the unit tier (about a minute; a fmt failure aborts the commit, so run `just cargo fmt --all` first). The pre-push hook runs `just test-prepush`; you never push.
2. You build on the phase branch **`feat/phase10-ux`** in the main checkout (the sponsor's clone; every `git` is `git -C` that path, every `just` is `just -d … -f …/justfile`). **First action:** `git -C … status --porcelain` must be empty and `git -C … log --oneline -1` must show this kickoff's freeze commit; stage files by name only, never `git add -A` or `git add .`. Never commit on `main`; never push; never tag.
3. Verified facts you may rely on (all at `88dd310` unless noted):
   - **Accepting writes an override, not a tree.** `Ops::stage_file` (`ops.rs:437`) CAS-checks the live path and calls `set_override(key, Some(oid), Some(mode))`; `stage_deletion` (`:447`) sets `blob: None` ("seen as absent"); `commit` (`:407`) locks, merges from disk, writes tmp + rename, and calls `compact` when `blob_override_count() > compaction_threshold`. `accept_all` (`:1235`) and `compact` (`:1251`) go through `fold` (`:1266`), which builds a new seen tree from the on-disk overrides plus the snapshot's rows and stamps `seen_at`. `accept_hunk` (`:544`) writes an override whose blob is baseline + the hunk. **The private store is never gc'd, pruned or repacked** (Amendment v1.2 item 2), so every blob and tree an accept ever referenced stays readable.
   - **Baseline resolution** (§6.2): `overrides[p].blob` if the override has a `blob` field (`null` = absent), else the blob for `p` in `seen_tree`, else empty. An explicit override always beats the tree. This is what makes undo compaction-proof: restoring a path's *previous baseline as an override* is correct whether or not a fold has since moved the accepted blob into the tree.
   - **`Override`** (`ledger.rs:201`): `blob: Option<Option<Oid>>`, `mode: Option<Mode>`, `flags: Vec<Flag>`, `updated_at`. **`Ledger`** (`:298`): `schema_version`, `root`, `kind`, `seen_tree`, `seen_at`, `overrides`, `unparsable`. `SCHEMA_VERSION` is `"1.1"` (`:33`); `deny_unknown_fields` is deliberately off (`:14`), so a 1.1 reader loads a file with extra fields and drops them on its next write. Ledger saves are `write_tmp` + `commit_tmp` (`:524`, `:534`) under `LedgerLock` (`:546`, bounded retry 40 × 50 ms).
   - **Listing** (`tui/app.rs:1125` `App::is_listed`): `loading.is_none() && !herdr.scope_pending && herdr.in_scope(path) && (!hide_empty || view.listed() || the herdr flag has attention)`. `hide_empty` (`:969`) is seeded from `hide_empty_repos` and flipped by `Action::HideEmpty` (`:3206`); it is session state, never written back. The header's `N repos` counts `listed_roots()`, so it follows every filter (§10 2026-09-10 verdict 1, KEEP).
   - **The confirm modal** (`app.rs:793` `ConfirmScope`, `:808` `Confirm`, `CONFIRM_ABOVE` = 10) and the flag note modal (`tui.md` "Flag (`m`), and the note modal") are the two existing modal shapes; the help overlay is `App::help: bool` (`:976`) drawn by `render_help` (`render.rs`). Their keys are not rebindable.
   - **herdr link state**: `HerdrView` (`tui/herdr.rs:245`): `link: Link` (`:177`, `Off | Connected { version } | Reconnecting | Standalone { reason }`), `scope: Option<Scope>`, `scoped: bool`, `scope_pending: bool`, `roots` flags. A herdr link is known once `scope_pending` is false.
   - **Keymap**: `DEFAULT_KEYMAP` and the action-name table in `tui/input.rs`; `Action::from_name` / `Action::describe` are the validation surface `lastcall config` and `lastcall tui` share. **Free unmodified letters: `c`, `s`, `x`, `z`** (`t` went to `hide_empty` in 9a). `shift-s` and `shift-z` are free. A new action needs its name in the table, a `describe` string, a `[keys]` row in `docs/config.md`, a `tui.md` "Keys" row and a help overlay row.
   - **Config**: `Config` (`config/mod.rs:50`) with `hide_empty_repos: bool`, `herdr: HerdrConfig` (`scope`), `update`, `keys`; `config_path(env)` (`:291`) resolves `$LASTCALL_CONFIG`, else `$XDG_CONFIG_HOME/lastcall/config.toml`, else `~/.config/lastcall/config.toml`; `load` (`:332`); unknown keys are a load error. **Nothing in the tree writes the config file today.** The workspace depends on `toml = "1"`; `toml_edit` is not in `Cargo.lock`.
   - **The state directory** (`paths.rs`, `Layout::state_dir()`): `$LASTCALL_STATE_DIR`, else `$XDG_STATE_HOME/lastcall`, else `~/.local/state/lastcall`; top level holds `roots/` and `update-check.json`. The update stamp (`commands/update.rs:790`, `Stamp`, `read_stamp`, `write_stamp` tmp + rename) is the idiom for a small state file the TUI owns.
   - **`status --json`** (`status_version` 1, `status.rs`): per root `root`, `kind`, `parent`, `badge`, `head`, `branch`, `remote`, `in_progress`, `seen_tree`, `seen_head`, `store`, `ledger_written_at`, `pending[]`, `omitted`, `groups[]`, `notices[]`. Golden `crates/lastcall/tests/golden/status_multi_repo.json`, regenerated by `just golden-update`.
   - **The PTY harness** (`lastcall-testkit/src/pty_tui.rs`) gives every scene a fresh temp state dir (`LASTCALL_STATE_DIR`) and `LASTCALL_KEYBOARD=plain`; the snapshot tier builds an `App` directly and renders to a buffer.
   - **Probes:** `just probe-tui` (the fixture parent); `just probe-tui-slow`.

## Deliverables

### 1. The first-launch overlay (sponsor item 1, ruling R1: Rec)

**What.** On the first `lastcall tui` ever run against a state directory, once the launch hold and the scope verdict are past, an overlay opens over the live screen: a keys card, then up to two cards that appear only when their condition holds, each offering to change one default and remember it. It is shown once; `lastcall tui --tour` shows it again.

**The marker.** `<state_dir>/first-launch.json`, `{ "shown_at": <unix seconds>, "version": "<binary version>" }`, written tmp + rename when the overlay is dismissed by any path (finished, skipped, or the process quit with it open: write it before taking the terminal down). Absent, unreadable or unparsable means "show"; headless commands never read or write it. `--tour` ignores the marker for that run and rewrites it on dismissal.

**When it opens.** At the first render where `loading.is_none() && !herdr.scope_pending` (the same instant `is_listed` first admits a root). Not before: the conditional cards need the link state and the empty-repo count. If the run ends before that instant, nothing is written and the next run shows it.

**The cards, in order.** Wording is final unless the reviewer or the sponsor changes it; keep the shape (a title, two to four lines, a footer of keys). House style: no em-dashes, no process vocabulary, no exclamation marks except the one in the herdr card.

1. **Keys** (always).
   Title: `Welcome to lastcall`.
   Body:
   `The list on the left is every repository under this directory. Pick a file and the diff opens on the right.`
   `a  accept the hunk under the cursor        A  accept the whole file        ^A  accept everything`
   `n / p  next and previous hunk               tab or the arrow keys move between the two panes`
   `u  put a hunk back the way it was          m  flag it with a note          z  undo the last accept`
   `t  hide repositories with nothing pending  s  snooze a repository          ?  every key, any time`
   Footer: `enter  next          q  skip the rest`
2. **herdr** (only when `herdr.link` is `Connected`).
   Title: `You are running inside herdr <version>. Nice!`
   Body:
   `lastcall follows this workspace. When a pane changes directory, the list narrows to the repositories that workspace is working in, and the bottom line says how many are out of view. w shows everything for the session.`
   Choice rows (the arrow keys move between them; enter applies):
   `> Keep following the workspace`
   `  Show every repository instead, and remember that   (writes scope = "all" under [herdr])`
   Footer: `enter  choose          q  skip the rest`
3. **Empty repositories** (only when at least **10** roots on the nav have nothing pending, `hide_empty` is off, and the config file does not already set `hide_empty_repos`).
   Title: `<N> of your <M> repositories have nothing pending`
   Body:
   `They are listed anyway so the picture is complete. t hides them for the session; the count on the bottom line keeps saying how many are hidden.`
   Choice rows:
   `> Keep listing every repository`
   `  Start with the empty ones hidden, and remember that   (writes hide_empty_repos = true)`
   Footer: `enter  choose          q  skip the rest`

**Keys on the overlay** (fixed, not rebindable, like the confirm modal): `enter` applies the selected row on a choice card or advances a plain card; `up`/`down` (and `k`/`j`) move between the two rows; `q` and `esc` skip the rest; every other key is ignored; the mouse selects a row on click. `q` here never quits the program; the overlay says `skip the rest`, and the help overlay's footer gains one line: `lastcall tui --tour shows the welcome again`.

**The one sanctioned config write.** Choosing the second row on a choice card sets exactly one key in the config file and applies it live in the same keystroke (`hide_empty = true` as if `t` had been pressed; `herdr.scoped = false` as if `w` had been pressed, and the config's `scope` value the next launch reads). The write:

- targets the path `config_path` resolves; when no file exists it creates `$XDG_CONFIG_HOME/lastcall/config.toml` (else `~/.config/lastcall/config.toml`) with the one key and a one-line comment `# written by lastcall's first-launch tour on <date>`;
- when a file exists, edits it **format-preserving** with `toml_edit` (add the dependency; MIT OR Apache-2.0; `just audit` must stay green): every other byte of the file, comments and ordering included, survives, proven by a unit test that diffs the file before and after against the one expected line;
- is tmp + rename beside the target;
- when the existing file does not parse, or the write fails, the card does not close silently: it replaces its footer with `could not write <path>: <reason>. Add this line yourself:` and the TOML line, and `enter` then advances. The live setting is still applied for the session.

**Rendering.** A centred box like the confirm modal, minimum 60 columns wide, the body wrapped to the box; at widths under 60 or heights under 14 the overlay is not shown and the marker is written (a tiny terminal is not the place for a tour; the help overlay covers the keys). Add snapshot scenes for every card at 100×30 and 80×24 (`tui_tour_keys`, `tui_tour_herdr`, `tui_tour_empty`), the herdr card with a `Link::Connected` view built directly, the empty card with 12 empty roots and 2 pending ones so the title reads `12 of your 14 repositories have nothing pending`.

**Harness.** Every existing PTY and snapshot scene must not change: `pty_tui.rs` writes the marker into each scene's fresh state dir before launch (a `tour: bool` on the scene builder, default off, deletes it instead), and the snapshot tier's `App` constructor takes the marker as already present. New PTY scenes: `tui_tour_first_launch` (fresh state dir, 12 empty roots: the keys card, `enter`, the empty card, `down`, `enter`, the nav now hides the empty roots, the header count drops, the config file's bytes are exactly the created file above, a second launch shows no overlay); `tui_tour_preserves_config` (a pre-written config file with `parent_dirs`, a comment, and a `[keys]` table: after the choice the file differs from the original by exactly one added line); `tui_tour_skip` (`q` on the first card: the marker is written, nothing else changes); `tui_tour_flag` (`--tour` with the marker present shows it again). The keys card and the two choice cards are reachable by the mouse in one hit-test unit test each.

### 2. Undo the last accept (sponsor item 2, ruling R2: Rec)

**What.** `z` (action `undo`) reverses the most recent accept in the selected root: a hunk, a file, a group, a deletion, an accept-all, or an editor save that blessed the file. A stack of the last **20** per root, persisted in the ledger so it survives a restart and is shared by every lastcall over the same state directory. Undo never touches the working tree; it only makes things pending again.

**The record.** Ledger schema stays `1.1`; `Ledger` gains an additive field, serialized last:

```json
"undo": [
  { "op": "accept_hunk | accept_file | accept_group | accept_deletion | accept_all | save",
    "at": "<iso8601>",
    "paths": { "<root-relative-path>": { "baseline": "<blob oid> | null", "mode": "100644 | 100755 | 120000 | null" } } }
]
```

`baseline` is what §6.2's resolution answered for that path **immediately before the op**, computed inside the same lock as the write (step 1 override blob, else step 2 tree entry, else `null`); `mode` likewise. The entry is pushed by the same `commit` or `fold` that writes the accept, so the two are one atomic ledger write. A refused accept pushes nothing. Cap: when the stack has 20 entries the oldest is dropped. An `accept_all` entry lists every path the fold took from the snapshot (the row cap already bounds that). `compact` pushes nothing (pile before == pile after). Restore (`u`, `shift-u`) and flag ops push nothing.

**The operation.** `Ops::undo(fault) -> Result<Outcome, OpsError>`: lock, merge from disk, pop the top entry, and for each path write `overrides[p].blob = Some(baseline)` and `mode` (an override whose blob is the previous baseline; `Some(None)` when the baseline was absent), **preserving the path's `flags` and stamping `updated_at`**; one ledger write. The stack being empty is a refusal (`Refused::NothingToUndo`), not an error. After the write, the next scan shows those paths pending with whatever their live diff is now; no CAS on the live content is needed or wanted, because a file that changed since the accept is exactly what the user wants to see again. A second lastcall that accepted the same path after this entry has its own entry on top; LIFO order makes that the one `z` pops.

**Compaction is not a boundary.** A unit test forces the threshold to 1, accepts three files (folding each), undoes three times, and asserts the pile equals the pile before the first accept. A second test does the same across two `Ops` over one ledger directory (the second opened after the first's writes). A third asserts a flag on a path survives its accept and its undo.

**UI.** `z` in either pane acts on the selected root (the root of the selected row, or the selected root row). Status line: `undid accept of <path>` for a single path, `undid accept of <n> files in <root>` otherwise, `nothing to undo in <root>` on refusal. After an undo the selection moves to the first path of the entry (path order) once the pile arrives, so the file reappears under the cursor; with the diff pane focused it stays focused. `ctrl-a` across roots pushes one entry per root; `z` undoes the selected root's top entry and the status line adds ` (<k> other repos have their own undo)` when other roots gained an entry from the same accept-all. Hint line: `z undo` at tier 1, shown only when the selected root's stack is non-empty. Help overlay row: `z  undo the last accept in this repository`.

**`status --json`**: additive per-root `undo` (integer, the stack depth); `status_version` stays 1; the golden is regenerated and the status doc updated.

**Tests.** Unit: the record's exact JSON shape; push on every accept family and on a blessing save; no push on refusal, restore, flag, compact; the cap at 20; the three tests above; a 1.1 file without the field loads with an empty stack. PTY: `tui_undo_file` (`shift-a` on a three-hunk file, then `z`: the file is back with three hunks, the status line says so); `tui_undo_accept_all` (30 files over two roots, `ctrl-a`, confirm, `z`: the selected root's files are back, the other root's are not, the status line names the other root). Snapshot: the hint line with `z undo` present and absent.

### 3. Snooze a repository (sponsor item 3, ruling R3: Rec)

**What.** `s` (action `snooze`) on a **repository row** in the nav hides that repository for a number of days, default 1; `shift-s` (action `show_snoozed`) shows the snoozed ones for the session; `s` on a shown snoozed repository wakes it. A snoozed repository is still watched and still scanned: like the workspace scope, snooze is a view, not a filter, and the headless commands are unaffected except for reporting it.

**The record.** `Ledger` gains an additive top-level field `snoozed_until: "<iso8601>" | null`, written under the lock by `Ops::snooze(until)` and `Ops::unsnooze()`; schema stays `1.1`. An expired value is treated as `null` by every reader and cleared on the ledger's next write. Draft roots snooze like git roots.

**UI.**

- `s` with anything but a repository row selected: status line `select a repository row to snooze it`, nothing else.
- `s` on a repository row opens a small modal in the note modal's shape: `snooze <name> for [1] day(s)`, the number editable with digits and backspace, range 1 to 365, `enter` applies, `esc` cancels. On apply: the ledger write, the repository leaves the nav, the selection moves to the entry that takes its place (§6.7 as amended by v1.9), the status line says `snoozed <name> until <YYYY-MM-DD>`.
- The bottom line's count: beside the existing scope notice, `· <k> snoozed (S shows)` whenever `k > 0`; when both a scope count and a snooze count exist they are separated by ` · ` and the line is subject to the same width tiers as today.
- `shift-s` flips a session flag `show_snoozed`; while on, snoozed repositories are listed with a dimmed suffix on the branch line, `snoozed until <YYYY-MM-DD>`, and `s` on one of them wakes it (status line `woke <name>`), no modal.
- A snoozed repository whose herdr flag has attention is listed regardless, exactly the `hide_empty` exception, with the same suffix.
- `is_listed` gains the clause; the header count follows it. Expiry is checked against the app clock at every pile and every render tick, so a snooze that expires while the TUI is open lists the repository again without a restart.

**`status --json`**: additive per-root `snoozed_until` (`null` when not snoozed or expired); `status_version` stays 1.

**Tests.** Unit: the field round-trips; expiry at the boundary; the modal's number editing and range; `is_listed` with snooze on, expired, and with attention; a draft root. PTY: `tui_snooze_repo` (`s` on a repository with pending files, `enter`: gone from the nav, the bottom line says `1 snoozed (S shows)`, quit, relaunch: still gone; `shift-s`: shown with the suffix; `s`: woke). Snapshot: the modal at 100×30; the bottom line with a scope count and a snooze count together at 100 and at 80 columns.

### 4. Reserved: the sponsor's bug

The sponsor has a bug to describe ("I do have another bug as well, but I'll need to explain that to you probably in the next message"). When it arrives it is added here as a dated orchestrator edit with its own tests; if it arrives after dispatch, the orchestrator decides whether it joins this phase as a resume message or becomes its own mini-phase.

### 5. Docs, CHANGELOG, the help overlay

- `docs/config.md`: `[keys]` rows for `undo`, `snooze`, `show_snoozed`; a new short section "The first launch" (what the tour is, what it may write, `--tour`); the `hide_empty_repos` and `[herdr] scope` rows mention the tour can set them.
- `docs/review-loop.md`: undo (one paragraph, under the accept walkthrough) and snooze (one paragraph, beside the `t` toggle).
- `docs/herdr.md`: one sentence in "Workspace scope" that the tour offers `scope = "all"`.
- `README.md` "What it does": one line each for undo and snooze, in the existing list's voice.
- `docs/dev/tui.md`: the tour (a new section under "Startup and the first frame"), the two keys, the snooze modal, the new snapshot and PTY scene names; `docs/dev/engine.md`: the two ledger fields, the three operations, the two status fields.
- `CHANGELOG.md`: a new `## Unreleased` section above `## 0.1.0 - 2026-09-12` with three H3s (`First launch`, `Undo`, `Snooze`), in the existing sections' voice; the release bump PR renames it.
- The help overlay: rows for `z`, `s`, `shift-s`; the footer line about `--tour`.
- `lastcall tui --tour` in the CLI help text.

## Gate (the checklist that closes this phase)

- [ ] **Tour:** the three card snapshots at 100×30 and 80×24; `tui_tour_first_launch` proves the overlay, the live change, the created config file's exact bytes, and the silent second launch; `tui_tour_preserves_config` proves a one-line diff on an existing file; `tui_tour_skip`; `tui_tour_flag`; the unit test for the parse-failure path showing the paste line; no existing scene changed for any reason but a deliberate hint or help row.
- [ ] **Undo:** the unit tests named in deliverable 2, the compaction and two-process tests included; `tui_undo_file` and `tui_undo_accept_all`; the status golden regenerated with `undo`; `just probe-tui`: accept a file, `z`, the file is back.
- [ ] **Snooze:** the unit tests named in deliverable 3; `tui_snooze_repo` including the relaunch; the two bottom-line snapshots; the status golden with `snoozed_until`.
- [ ] **Docs** per deliverable 5, every new key in `docs/config.md`, `tui.md` "Keys" and the help overlay; `just lint` (the docs greps included) green; house-style grep on the overlay's strings: no em-dash, none of sponsor/orchestrator/verifier/worker/gate/ruling.
- [ ] **Dependency:** if `toml_edit` is added, `just audit` green and `deny.toml` unchanged, or the change justified with a reason and a date.
- [ ] Standing: unit floor 604 grows (state the new count and its split in the report); lint and hooks per commit; e2e counts stated; the real-herdr subset untouched and green (`just test-integration-herdr`); prepush green; `just probe-tui` and `just probe-tui-slow` looked at once each after the tour lands (the overlay must not paint over the launch hold).
- [ ] **[sponsor]** the tour on his own machine (`lastcall tui --tour` in his parent directory), one `z` after a real accept, one `s` on a real repository. Recorded in §10 with his words.
- [ ] Amendment v1.11 proposed in `00-spec.md`, ratified by the merge; §10 close-out entry; the orchestrator's judgment-call list.

## Operational rules

- Sacred: `~/.local/state/lastcall`, `~/.config/lastcall`, `~/.config/herdr`, `z_ignore/`. Every test and probe sets `LASTCALL_STATE_DIR`, `LASTCALL_CONFIG` and `XDG_CONFIG_HOME` to temp directories: **the config-write tests must never be able to reach the sponsor's real config file**; a test that runs with `XDG_CONFIG_HOME` unset and `LASTCALL_CONFIG` unset is a defect. The reference clones `~/dev/git/herdr` and `~/dev/git/drydock` are read-only.
- Frozen artifacts: `00-spec.md` §5, §6 and `01-scenarios.md` change only through Amendment v1.11 as proposed here; anything beyond it is STOP and report. `deny.toml`, `release.yml` untouched.
- Workspace rules: `unsafe_code = "forbid"`, no direct `libc`; the pre-commit greps enforce them.
- Commit discipline: one commit per deliverable at least, conventional subjects, resumable from git alone; the version in `Cargo.toml` stays `0.1.0` (the bump is release-sequence work).
- Never run two cargo processes at once; the orchestrator runs none while you build.
- NEVER end your turn to wait for anything; no notification will come. Poll bounded cycles in-turn.

## Working agreements

- Grounded progress claims: test names and counts, file paths and line numbers, the bytes of the written config file, never "it works".
- Verifier (fresh context, after the build) hunts: (1) the tour reaching a real config file or painting during the launch hold; (2) an undo that touches the working tree, or leaves a path's baseline different from the pre-accept one after a compaction; (3) undo dropping a flag; (4) a snoozed repository with attention staying hidden, or an expired snooze staying hidden until restart; (5) a snapshot that changed for no deliberate reason; (6) a key added without its `describe`, doc row and help row; (7) a docs claim the code does not back.
- Stop conditions: gate met; a frozen-artifact temptation beyond v1.11; a genuine human-only blocker. Commit clean, write the report, stop.

## Final report

Self-assessment per gate item with evidence; the new unit count and split; the e2e counts; every decision you took that the spec did not make, numbered, split into needs-a-ruling and informational; the exact bytes the tour wrote in `tui_tour_first_launch`.
