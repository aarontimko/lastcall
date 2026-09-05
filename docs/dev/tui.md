# The TUI

`lastcall` with no subcommand (or `lastcall tui [--poll <secs>]`) opens the Phase 3 Ratatui
app: a read-only view of every watched root's pile that updates live as files change. It
lives in `crates/lastcall/src/tui/` (`term`, `app`, `input`, `render`, `run`) with the CLI
wiring in `crates/lastcall/src/commands/tui.rs`. Kickoff: `docs/spec/92-phase3-kickoff.md`.

Invariant 9 holds throughout: the TUI reads engine piles and root metadata only. Nothing
under `tui/` reads a file, a ledger or git, and it holds no review state of its own (the
gate greps at the end of this page are how that is enforced).

## Architecture: the screen is a function of state

```
 engine watcher ──EngineEvent──▶ App::apply ─┐
 terminal (reader thread) ──Event──▶ to_action(Event, &Keymap) ──Action──▶ App::handle ─┤
 loop-owned engine work ──Local──▶ App (apply_pile / sync_roots / refresh_done) ────────┤
                                                                                        ▼
                                                     (Changed, Option<Effect>)   ──▶ draw?
                                                                                        │
                                              render(&App, frame) -> HitMap  ◀──────────┘
```

- **`app.rs` — `App` is a pure value.** `apply` folds engine events (`Pile`, `Head`,
  `Notice`, `RootsChanged`, …) into it, `handle` folds user `Action`s, `sync_roots` folds
  the engine's root list, `accepted` folds the results of an accept. Every reducer returns
  `(Changed, Option<Effect>)`: `Changed::Yes` is the only thing that triggers a redraw, and
  an `Effect` (`Refresh`, `SyncRoots`, `Quit`, `Accept(requests)`) is the only way the app
  asks the loop for work — the reducer itself does no I/O. The same
  event sequence always yields the same `App` (`app_unchanged_pile_is_no_change` feeds one
  pile twice and asserts nothing changed). Selection is by root path and row path *bytes*,
  never by index, so a rescan that reorders or removes rows can only move it through
  `reconcile_selection`'s documented fallback (row → its root entry → nothing). Piles that
  arrive before their root's metadata are parked in `orphan_piles` and adopted on the next
  `sync_roots`. `App.now` advances only on `Action::Tick`; render computes status-line ages
  from it, never from `Instant::now()`, which is why two renders of one `App` are identical.
- **`render.rs` — `render(&App, frame) -> HitMap`.** Reads nothing but the app and the
  frame's own area: no clock, no engine, no files. Layout: a one-line header (`lastcall  N
  repos · N files · N hunks  [Accept All]` plus the watch notice on the right; the file
  count reads `N+` when any root's pile stopped at the engine's row cap; when the control
  and the notice do not both fit — 60 columns — the control is dropped and the notice
  kept, since `^A` duplicates the control and nothing else says what is watched), the body — a nav pane
  (outer width `App.nav_width`, 16..=60, default 28; hidden below `NAV_MIN_COLS` = 70
  columns, when the diff takes the whole body and has focus) sharing its right border with a
  bordered main pane — and a one-line status bar (the latest engine notice with its age for
  `app::STATUS_TTL` = 30 s, then the key hints). Below `MIN_SIZE` (40×10) the whole frame
  is `render::TOO_SMALL` (`too small: 40×10 min`) and the hit map is empty. Only the visible
  window of nav entries and diff lines is built, so a 50 000-line diff costs the same as a
  50-line one. In the diff, one blank line separates consecutive hunks — none before the
  first, none after the last — and the selected hunk's `@@ … @@` header is a full-width
  band across the pane (its `[a accept]` control inside the band, so it is plain which hunk
  the control takes); `app::hunk_block` is that geometry, and every scroll, offset and
  `diff_len` counts the separators (`render_hunks_are_separated_and_the_current_header_is_a_band`).
  The help overlay (`?`) and the accept confirm modal are drawn last over
  everything.
- **`input.rs` — `Action` and the keymap.** Every key, mouse gesture and the 1 s tick becomes
  one `Action` before it touches `App` (`to_action(&Event, &Keymap)`), so the reducer never
  sees a crossterm type and the keyboard and mouse paths are provably equivalent — the seven
  parity tests `input_parity_select_repo`, `input_parity_select_file`,
  `input_parity_hunk_next`, `input_parity_hunk_prev`, `input_parity_accept_hunk`,
  `input_parity_accept_file`, `input_parity_accept_all` drive the same scene by key and by a
  click resolved through the hit map and assert the same `App` (and, for the accepts, the
  same `Effect`). `input_parity_arrows_match_h_and_l` is the same shape for two spellings of
  one key: `right`/`l` and `left`/`h`. The confirm modal's keys (`input::MODAL_KEYS`: `y`/`enter` confirm,
  `n`/`esc` cancel) are not in the keymap: `Ui::event` resolves them through `modal_action`
  before the keymap while the modal is open, lets only the keymap's `quit` keys through
  after that (`q` and ctrl-c quit everywhere, as through the help overlay), and swallows
  every other key.
- **`run.rs` — the loop.** One tokio `select!` over the watcher's events, the terminal
  reader thread's events, the loop's own finished engine work (`Local`), a 1 s tick, Ctrl-C
  (a key event under raw mode; the signal branch is for `kill -INT`) and SIGTERM. One
  **pass** per iteration, not one per event: see "One draw per pass" below. Every
  engine call goes through `watcher::blocking` on a spawned task; the UI task never holds the
  engine mutex (`rg -n 'lock\(' crates/lastcall/src/tui` finds nothing). `Effect::Accept`
  runs as **one** `blocking` closure over every root it covers (`spawn_accept`): each root's
  `Engine::accept` — the op and its rescan — happens under the same hold of the engine
  mutex, so no watcher scan interleaves, and the results come back as one `Local::Accepted`.
  The `Ui` wrapper (`event` / `engine` / `local` / `rendered`) is the unit-testable half of
  the loop.
- **`term.rs` — the terminal lifecycle.** `enter()` installs the panic hook *before* raw
  mode, then raw mode + alternate screen + mouse capture (bracketed paste stays off).
  `restore()` is idempotent, uses only global crossterm commands on stdout, and is shared by
  the panic hook, `TerminalGuard::drop` and the quit path, so a crash never leaves the shell
  in raw mode.

### One draw per pass (Phase 6)

The `select!` wakes on one event; `run::drain` then polls the other three receivers
round-robin — input, engine, local, herdr — and folds everything already queued into the
same **`Pass`** before the frame is drawn. Ten piles landing together are one draw, not
ten, and a herdr resync that re-derives an identical root map is now no draw at all
(`ReadyDelta.changed`, `herdr_apply_roots_reports_changed_only_when_the_map_moved`, and
`app_herdr_roots_draw_nothing_when_the_derivation_is_identical`).

- **`Pass { changed, cause, effects, stop, rescan, held }`.** `changed` is the `Changed`
  fold over every event in the pass; `cause` is the **first** source that made it
  `Changed::Yes` (`"input"`, `"engine"`, `"local"`, `"herdr"`, `"timer"`, `"tick"`) and is
  the `cause=` field of the `draw` probe; `effects` are the non-quit effects, run in order
  after the drain. A quit — from `Pass::of`'s own seed or from any folded event — becomes
  `Pass::stop`, never an entry in `effects`: the dispatch loop must not `break` out of a
  `for` and drop the rest of the pass's work, and the seeded case is asserted
  (`run_drain_stops_at_quit_and_at_a_fatal`). `Local::Fatal` becomes `Stop::Fatal` the same
  way.
- **`DRAIN_CAP` = 256** round-robin rounds per pass — each round polls the input, engine,
  local and herdr queues once, so a pass folds at most 1,024 events. Past it the drain
  returns with the rest still queued, so a pathological producer cannot starve the frame
  (`run_drain_stops_at_the_cap`).
- **A drained `worktree.*` event arms the 500 ms discovery debounce** exactly as one that
  woke the `select!` does (`Pass::worktree`); only the timer firing rescans. Otherwise the
  second event of a checkout burst, drained behind the first, would rescan at once and
  cancel the timer the first had armed
  (`run_drain_reports_a_worktree_event_for_the_debounce_not_for_a_rescan`).
- **Press pushback.** A left `Press` that arrives once the pass is already
  `Changed::Yes` is **held**, not folded: it would be hit-tested against a `HitMap` from a
  frame the user never saw. It is replayed as the first event of the next pass, against the
  frame it was aimed at (`run_drain_holds_a_press_behind_an_undrawn_change`). Wheel and
  release events have no such hazard and fold freely.

`run_drain_folds_a_burst_of_piles_into_one_pass` is the shape test: 17 piles pushed into
two different receivers, one pass, 17 hunks on the row, and an empty `Pass` on the next
drain.

### The nav keeps its scroll offset (Phase 6)

`App.nav_top` is the nav's offset in **nav lines** — the vector `render_nav` builds and
windows, not `nav_entries()`. No reducer ever writes it
(`app_reducers_never_move_the_nav_offset`): `render_nav` clamps the stored offset to the
current list, scrolls it the *minimum* needed to bring the selection into view, and reports
where it landed as
`HitMap::nav_top`, which `Ui::rendered` writes back. `HitMap::nav_top` is `None` whenever
the nav was not drawn (below `NAV_MIN_COLS`), and then the offset survives untouched —
`run_nav_offset_is_written_back_only_by_a_frame_that_drew_the_nav`. The visible
consequence: clicking a row that is already on screen no longer scrolls the list, and a
selection that moves one line moves the window one line.

### Startup and the first frame

`commands/tui.rs` fails loudly *before* the terminal is touched: stdout not a TTY → the
one permitted message `lastcall: not a terminal; try \`lastcall status\`` on stderr, exit 2
(never draws into a pipe); a `[keys]` table that does not parse → `lastcall: [keys] …`, exit
2; an engine that cannot open → exit 1 like `status`. Discovery itself can take seconds on a
large parent dir (S1's hundred roots: about 3.7 s), and it happens *before* there is a
screen to draw into, so `commands/tui.rs` prints one line to **stderr** first —
`lastcall: discovering roots under <dir>[, <dir>]…` (`commands::tui::DISCOVERING`) — after
`term::init_tracing()` and before `Engine::open`. It is the only thing on stderr in the
happy path, it scrolls away with the shell's scrollback when the alternate screen opens,
and the PTY harness pins the order: the line, then `\x1b[?1049h`, then `scanning N roots…`
(`wait_first_piles`). Then `run::run` builds the runtime as
`watch` does, enters the terminal, seeds the app with the engine's roots (`sync_roots`) and
the status `scanning N roots…`, and draws the empty state — the first piles arrive through
the watcher a moment later (about 1.6 s on the fixture under the PTY harness). `--poll N`
shortens the HEAD-poll and rescan backstops exactly as for `watch` (`just probe-tui` uses
`--poll 1`, the deterministic setting on a host whose FSEvents are unreliable).

### Quit, in the only safe order

`Effect::Quit` (from `q`, `ctrl-c`, or a SIGTERM) ends the loop; then `shut_down` runs
`restore()` **first** — the shell is sane even if the rest hangs — then a bounded
`watcher.join()` (500 ms) and a bounded runtime shutdown (500 ms). The reader thread is never
joined: `crossterm::event::read()` blocks in `mio` on the tty, so it polls with a 50 ms
timeout under a stop flag and exits on its own. `run_shutdown_restores_before_joining_before_runtime_shutdown`
pins the order; the PTY scenes measure and print it (a few hundred milliseconds after `q`
or Ctrl-C on the fixture).

## The accept loop

Every accept on screen is the Phase 2 engine's compare-and-swap (`00-spec.md` §6.3) driven
from what the user is looking at:

- **Scope** (`App::accept_scope`, `AcceptScope`): `a` on a file row is the **one hunk**
  under the diff cursor — whichever pane has focus, so `a` from the nav never takes a whole
  file (`app_accept_from_the_nav_pane_takes_one_hunk_not_the_file`). The one carve-out is a
  file row with no hunks to point at (binary, collapsed, deleted, unreadable): there `a`
  still takes the row whole. On a group entry `a` is the group; on a root entry, every row
  of that root (the per-repo fold). `A` is the only key that takes a whole file, from either
  pane; `ctrl-a` and the header's `[Accept All]` are every listed root.
- **Requests are built from the held rows, never from the engine.** `accept_requests`
  makes one `(root, AcceptRequest)` per root covered, with `Rendered::of` on the `App`'s own
  `Row` (`rg -n 'Rendered::of' crates/lastcall/src` finds only `app.rs`) and, for a fold,
  `AcceptRequest::All(view.pile.clone())` — exactly the pile that was rendered, so the
  engine blesses what was shown and refuses (`Refused::Moved`) anything that changed since.
- **One at a time.** `App.accepting` holds the scope in flight; a second accept while one
  runs is ignored with the status `accept in progress`, and the modal never opens while one
  is running (`Confirm` re-checks, so a confirm is never silently dropped or doubled).
- **Completion** (`App::accepted(Vec<(root, Result<Accepted, String>)>)`): each root's
  pile goes through the same `apply_pile` path as a watcher pile (seq included); then the
  advance rule; then one status line — `accepted f1 · 2 hunks left` (one fewer than the
  row showed when the accept was asked; the cursor's slot is always 1 again once the
  accepted hunk slides out, so it is not quoted), `accepted f1 · file complete` when that
  was the file's last hunk,
  `accepted f1`, `accepted f3 (deleted)`, `accepted upstream · 4 files`, `accepted 12 files
  in alpha`, `accepted 30 files in 3 repos`; on refusals the `Refused` texts joined by
  ` · ` (the first only plus ` (+N more)` beyond two); an `Err` for one root is reported as
  `alpha: <error>` and does not undo the others.
- **Advance (§6.7).** After a hunk accept with hunks left, the cursor keeps its index
  (clamped — the next hunk slides into it) and the scroll follows. When the accepted
  selection is gone from the nav — the row's last hunk, a whole file, a group, a fold —
  `advance(root, path)` picks the next row by path after the accepted one in the root's
  new pile, else the first remaining row of that root, else the first *row* of the next
  listed root, else nothing; a root whose pile emptied is unlisted (the existing rule).
  Focus stays where it was. A refusal (or an error) leaves the selection *and the diff
  cursor* where they were — the scroll does not snap back to the hunk header
  (`app_refused_hunk_accept_leaves_the_scroll_alone`).
- **The confirm modal.** An accept covering more than `CONFIRM_ABOVE` = 10 files asks
  first (10 accepts, 11 asks). `App.confirm` stores only the scope; the numbers shown are
  `confirm_counts()` from the held piles at *every* render, so a pile applied under the
  open modal changes them and `Confirm` folds exactly what is shown (if the scope empties
  underneath, the modal closes with `nothing to accept`). While the modal is open every
  action but `Tick`, `Resize`, `Confirm`, `Cancel`, `Quit` is ignored, and every key but
  its own and the `quit` keys is swallowed before the keymap — so `Esc` cancels without
  also going back, and `q` / ctrl-c still quit (the modal is never a trap).
- **The seq rule (the §11 hardening).** `App.seq` remembers the last scan seq applied per
  root; a `Pile` event with a lower seq — a watcher scan that was already running when the
  accept took the lock — is `Changed::No` and touches nothing, through either channel
  (`app_older_seq_pile_is_dropped_untouched`,
  `run_stale_watcher_pile_after_accept_is_dropped`). The entry is removed when the root is
  removed, so a re-added root receives piles again.
- **Hints follow the selection** so the per-repo fold and the global one are told apart:
  `a accept hunk  A accept file` on a file row **in both panes**, `a/A accept file` on a
  hunkless file row, `a accept group`, `a accept all in <root>`, and `^A accept all`. When
  the line would not fit it drops `Tab focus  r refresh` first (always below 70 columns),
  then the file and global accept hints. While the confirm modal is open the line is
  `y confirm  n cancel  q quit` — exactly the keys that work there, the `quit` label being
  the user's own binding (`render_hint_line_under_the_modal_names_only_its_keys`).

## Restore and flag (Phase 7)

Accept is one of the three answers a reviewer has. The other two are "put that back" and
"I have a question about this" — `u` / `shift-u` and `m` / `shift-m`.

### Restore (`u`, `shift-u`)

- **Scope** (`App::restore_scope`, `RestoreScope`): the same shape as `accept_scope` with two
  variants instead of five. `u` on a row with content hunks is the hunk under the diff
  cursor; on a hunkless row (binary, collapsed, unreadable) it is the file; on a **deletion**
  row it is also the file, because a deletion row's single hunk *is* the file (F16). `shift-u`
  is always the file. There is no restore-group and no restore-all: undoing a whole tree at
  once is not a gesture lastcall offers, which is why `Effect::Restore` carries a `Vec` that
  never holds more than one request.
- **Only the file half asks.** A hunk restore starts immediately; a file restore opens the
  confirm modal first. The CAS is the guard either way and the content a restore drops stays
  addressable in the private store, but a whole file going back is the bigger surprise. The
  modal is the accept modal with a different scope — `ConfirmScope::{Accept, Restore}` — so
  there is one modal, one key set and one hint line. `confirm_counts` stays accept-only
  (F11): a restore covers one row, so there is nothing to tally, and the question comes from
  `restore_question(scope)`, the only place its wording lives:

  | row | question |
  |---|---|
  | modified, 3 hunks | `Restore f1 · 3 hunks?` |
  | deleted | `Restore f1? (deleted)` |
  | added since the baseline | `Delete f1? (added since baseline)` |

  The third is not a euphemism to soften: restoring a file the baseline does not have
  **removes** it, and the status line afterwards says `removed f1 (added since baseline)`.
- **One at a time**, like accept: `App.restoring` holds the scope in flight and a second
  restore is ignored with `restore in progress`.
- **Completion** (`App::restored`): the pile goes through `apply_pile` (seq included), the
  §6.7 advance rule runs for the selection the restore was asked from, and one status line
  says what happened — `restored f1 hunk 2`, `restored f1`, `removed f1 (added since
  baseline)`. Refusals read with their own verb (`Refused::message("restored")`), because a
  sentence has to say which operation did not happen.
- The TUI **never opens a worktree file for writing**. Every byte a restore writes goes
  through `Engine::restore`, and every flag through `Engine::flag`/`unflag`:
  `rg -n 'e\.(restore|flag|unflag)\(' crates/lastcall/src` finds only `run.rs`'s effect
  handlers. The one file the TUI opens for writing is the export fallback under the state
  dir (below).

### Flag (`m`), and the note modal

`m` opens the note modal on what `App::flag_target` names: the hunk under the diff cursor
when the diff has focus and the row has content hunks, else the file. The synthetic mode
hunk is not content — there is nothing to quote — so `m` on it flags the file.

**The target is captured when `m` is pressed and never re-read** (F14). Piles keep landing
while the note is being typed: an agent still writing can reorder the hunks or remove the
one the note is about, and the flag must not follow. `NoteEntry.target` holds the `Rendered`
row, the `FlagHunk` (index, header, body) and the `of` count as they were on screen; Enter
writes exactly that.

The modal's key discipline (`input::note_action`):

| key | effect |
|---|---|
| any printable character | inserted — the keymap is off, so `q` types a `q` |
| `Enter` | send: `Effect::Flag`, the modal closes |
| `Ctrl-J` | newline (works in every terminal) |
| `Alt-Enter`, `Shift-Enter` | newline, where the terminal reports the modifier at all |
| `Backspace` | delete the character before the caret |
| `Esc` | cancel — nothing is written |
| the `quit` binding, non-printable only | quit (`Ctrl-C` by default): a modal is never a trap |
| anything else | swallowed |

**Bracketed paste is on for the modal's lifetime and no longer.** The loop enables it when
`app.note` becomes `Some` and disables it when the modal closes, so a paste arrives as one
`Event::Paste` carrying every newline it holds — inserted whole, never mistaken for the
`Enter` that sends. A pasted multi-line note that fired off its first line and dropped the
rest would be the worst failure this modal has, and the paste event is what prevents it.
Mouse clicks are ignored while the modal is open: there is nothing on it to click.

### The send decision

The flag is written first — `Effect::Flag` → `Engine::flag` → `Local::Flagged` — and only
then does the loop decide where the export goes, from the candidates the last herdr
derivation found for that root (`HerdrView::candidates`, narrowed by the `w` scope when it
is on, so the picker covers the ground the nav does):

| candidates | what happens |
|---|---|
| exactly one | `Effect::Stage` at once — there is nothing to ask |
| more than one | the picker modal; `↑↓`/`kj` choose, `Enter` sends, `Esc` drops the send |
| none, or standalone | `Effect::Export` — the fallback file |

lastcall never picks an agent for the reader. **The flag is on disk before any of this**, so
`Esc` on the picker loses nothing: the status says `flagged f1 · not sent` and the row keeps
its `⚑`. The picker is live — a pane that appears or goes away while it is open changes the
list under the cursor, and the selection is clamped to it; every candidate going away closes
it rather than showing an empty list.

**Staged, not sent.** `herdr::stage` wraps the export in bracketed-paste markers and calls
`pane.send_text`, so the payload lands in the agent's input buffer and waits for the human
to press Enter. No trailing newline — that would be the submit we are avoiding. A send that
fails is a status line and nothing more (`flagged f1 · send failed: <reason>`): the flag is
in the ledger either way, which is why `App.flagging` holds the flag's label until `staged`
answers.

**The fallback file** is the one file the TUI writes:

```text
<state_dir>/exports/<root basename>/<YYYY-MM-DD>.md
```

Appended to, never truncated, with a blank line between entries; the date comes from the
**engine's clock**, not `SystemTime::now()`, which is what lets a test with a `FixedClock`
name the file it expects. The status line names the path it wrote:
`flagged f1 · export → /…/exports/alpha/2026-09-05.md`.

`shift-m` (`unflag`) clears **every** flag on the selected file — Phase 7 has no per-flag
removal — and says `flags cleared`.

### Where flags show

- The nav row carries `⚑` for one flag and `⚑2` for two or more: the count is the only thing
  that says a row has more than one note without opening it.
- The file header and the flagged hunk's header carry `⚑ <the note's first line>`, dimmed.
  Only the first line, and only in the room left once the right-aligned controls are
  reserved (`render::marker_budget` / `flag_marker`) — a note is whatever the reviewer typed,
  and a flagged row is exactly the one whose `[u restore]` and `[m flag]` they still want.
- The hunk marker matches on the **header text** the flag stored, not on its index: the index
  is where the hunk was when it was flagged, and one edit above it moves every later hunk
  down. A flag whose header no longer appears simply shows no marker rather than marking the
  wrong hunk.

## Keys

Defaults (`input::DEFAULT_KEYMAP`, in help-overlay order):

| action (the `[keys]` name) | default keys | in the nav | in the diff |
|---|---|---|---|
| `nav_up` / `nav_down` | `up` `k` / `down` `j` | previous / next entry | scroll one line |
| `nav_page_up` / `nav_page_down` | `pageup` `b` / `pagedown` `space` | a page of entries | a page of lines |
| `open` | `enter` `l` `right` | open the selected row's diff, cursor on that file's current hunk (on a root: its first row) | — |
| `back` | `esc` `h` `left` | — | back to the file list with the same row selected; closes help first; never quits |
| `focus_toggle` | `tab` | toggle focus between the panes | |
| `hunk_next` / `hunk_prev` | `n` `]` / `p` `[` | next / previous hunk (the current hunk's header is a full-width inverted band) | |
| `toggle_full_paths` | `f` | root-relative paths instead of basenames | |
| `toggle_remote` | `o` | show each repo's `org/repo` slug | |
| `accept` | `a` | on a file row: the one hunk under the diff cursor (a hunkless row — binary, collapsed, deleted, unreadable — whole); on a group: the group; on a root: every row of it (asks above 10 files) | the same hunk |
| `accept_file` | `shift-a` | accept the selected file whole — the only key that does | |
| `accept_all` | `ctrl-a` | accept everything listed, every root (asks above 10 files) | |
| `restore` | `u` | put the hunk under the diff cursor back to its baseline (a hunkless or deleted row: the file, which asks) | the same hunk |
| `restore_file` | `shift-u` | put the selected file back whole — always asks first | |
| `flag` | `m` | flag it with a note: the hunk under the diff cursor, or the file from the nav | the same hunk |
| `unflag` | `shift-m` | clear every flag on the selected file | |
| `expand` | `e` | expand the selected collapsed row into hunks ("Collapsed rows" below) | |
| `ack` | `d` | ack the selected root's herdr ready flag ("herdr in the UI" below) | |
| `jump` | `g` | focus the selected root's agent in herdr | |
| `scope` | `w` | workspace scope on/off | |
| `refresh` | `r` | rescan every root now (ignored while one is running) | |
| `help` | `?` | the help overlay (any key closes it) | |
| `quit` | `q` `ctrl-c` | exit 0 | |
| `scroll_up` / `scroll_down` | *(unbound)* | bindable one-line diff scrolls | |

**Focus moves horizontally.** The two panes sit side by side, so the arrows move between
them: `right` (= `enter` / `l`, action `open`) on a selected file row focuses the diff with
the cursor on that file's current hunk; `left` (= `esc` / `h`, action `back`) returns focus
to the nav with the same row still selected. They are ordinary third specs of `open` and
`back`, not a separate path — `input_parity_arrows_match_h_and_l` drives one scene both ways
and asserts the same `App` and the same frame — and `[keys]` overrides them like any other
binding (`keymap_back_can_be_rebound_to_left`). `tab` (`focus_toggle`) still flips focus
without touching the selection, and a hidden nav (below 70 columns) can never hold focus.

The confirm modal answers `y` / `enter` (confirm) and `n` / `esc` (cancel) — fixed
(`input::MODAL_KEYS`), not `[keys]` names, listed last in the help overlay — plus the
`quit` keys, which quit from inside it; nothing else. The note modal and the agent picker
have their own key sets, printed on the modal itself ("Restore and flag" above).

**The help overlay is two columns when one does not fit.** With 27 bindable rows plus the
modal keys, a single column runs off the bottom of a 30-row terminal, so
`render::help_columns` splits the rows in half whenever one column would overflow the height
*and* the pair fits the width — each column sized to its own widest row, because padding both
to the widest row in the table costs the second column the width it needs. If two columns
would themselves have to be truncated, one column is no worse, and it stays. The vertical
clipping that follows eats key rows, never the footer, so the shift-drag note
(`render::SELECT_NOTE`) is always the last line of the box.

Mouse: a left press on a nav entry selects it; on a hunk header it selects that hunk; on a
hunk header's `[a accept]` it accepts that hunk, on `[u restore]` it restores it and on
`[m flag]` it opens the note modal on it; on the main view's `[A accept file]` /
`[U restore file]` the file, on the header's `[Accept All]` everything listed; on the diff body it focuses the
diff; dragging the divider resizes the nav (clamped to 16..=60); the wheel scrolls the pane
under the pointer, three lines a notch.

**Selecting text.** `term::enter` turns mouse capture on, so a plain drag is ours, not the
terminal's. Hold **shift** while dragging to select and copy with the terminal's own
selection (every terminal we target honours the shift override). The help overlay says so
in its last line (`render::SELECT_NOTE`); it is the stopgap until the Phase 8 select-to-copy
item lands.

### Collapsed rows and `e` (Phase 6)

A lockfile, a binary or a file over `collapse_size_bytes` is a **collapsed** row: `⊟` in the
nav, and in the main pane one dimmed line instead of a diff —
`collapsed (glob|binary|size) · +a −d` — with `[e expand]` right-aligned on it. The accept
story is unchanged and deliberately whole-row: `a` on a collapsed row takes the file (there
is no hunk to point at) and `A` does the same, which is why an expansion draws **no
per-hunk `[a accept]` control** — the row header's `[A accept file]` is the only accept on
that screen.

- `e` (or a click on `[e expand]`) emits `Effect::Expand(root, Box<Row>)`; the loop runs
  `Engine::hunks_of` off the UI task and hands the result back as `Local::Expanded`. The
  boxed row is not ceremony: `hunks_of` diffs *that row's* oids, so the expansion always
  matches the counts on screen.
- `App.expanded: Option<Expansion>` holds `{ root, path, baseline, current, view }` — one
  at a time, and `App::expansion()` returns it only while the selection still points at
  that row. A newer pile whose oids differ clears it, and an answer that lands after such
  a pile is dropped rather than stored — the request's row travels back with
  `Local::Expanded` and `set_expanded` compares its oids to the row's — so a stale
  expansion cannot outlive the delta it was computed from
  (`app_expansion_answer_for_moved_oids_is_dropped`).
- A mode-only change on a collapsed row has no content hunks; the header names it
  (`collapsed (glob) · +0 −0 · mode 100644 → 100755`) and `e` shows the synthetic mode
  hunk.
- **Binary is never expandable.** The line reads `collapsed (binary) · +a −d · not
  expandable`, no control is drawn and no hit target is registered, and `e` on such a row
  is a silent no-op — no effect, no redraw. `e` is equally silent on a row that is not
  collapsed and on one already expanded (`app_expand_is_a_no_op_off_a_collapsed_row`), and
  the key and the click go down the same path (`input_parity_expand`, whose second half
  asserts a binary row offers no expand target).
- The expansion is capped at `hunks::EXPAND_LINE_CAP` = 2,000 body lines. When it truncates,
  the **last line of the pane** is `… N lines omitted (cap 2,000)` — the footer owns that
  line, so a truncated expansion can never scroll its own warning off the screen.

**Design-pass input** (§10 2026-09-05 ruling 3: Phases 6–8 add no new layout concept
without a note here naming it). Phase 6 adds three: the **collapsed-row body** — a dimmed
status line carrying a right-aligned `[e expand]` control, with hunks and a cap footer
under it — the **retained nav offset**, which changes when the list scrolls rather than
what it looks like — and the **help overlay's height**: the `expand` row makes it 31
entries, so on a 30-row terminal the box now starts on the header row
(`tui_help_overlay`); one more action (Phase 7's flag and restore) covers the hint line and
two more clip `any key closes`, so the overlay's sizing is pass input, not just its rows. The snapshots `tui_draft_root_hunks`, `tui_nav_collapsed_lockfile`,
`tui_nav_collapsed_binary_and_size` and `tui_diff_view_collapsed_expanded` are those
frames, and they join `tui_accept_controls`, `tui_herdr_scope_notice` and `tui_herdr_scope_notice_with_status` as
the pass's input at the Phase 9 kickoff. Nothing in the status line, the header ladder or
the scope notice moved.

Phase 7 answers that sizing question rather than deferring it: the overlay is **two
columns** when one column does not fit. `render_help` builds the key rows, then
`help_columns` measures them — one column stands while `rows + 2 + 4 <= area.height` (the
`+ 2` is the blank line and `SELECT_NOTE`, the `+ 4` the border, the pad and the `any key
closes` line); past that it splits the rows in half with `div_ceil`, pads column one to its
own widest row plus `HELP_GUTTER` (3 spaces) and joins column two beside it. Reading order
runs **down column one, then down column two** — not across — so the keymap's order is
still the order you read. If the two columns plus the border would not fit the width, the
overlay stays one column and clips as before: narrow beats scrambled. `SELECT_NOTE` and
`any key closes` are never columnised; they stay full width under the body. The frames are
`tui_help_overlay` (100×30) and `tui_help_overlay_tall` (100×45): 23 rows still fit one
column at 30 lines, and Phase 7's four new keys (`u`, `U`, `m`, `M`) take it to 27, which
is where the split starts — the tall frame is the control that keeps one column pinned.
The pair is the design pass's input on whether a two-column box is the right answer for a
keymap that keeps growing.

### The `[keys]` table (`config.toml`)

```toml
[keys]
quit = "ctrl-q"            # one spec …
hunk_next = ["n", "ctrl-n"] # … or a list; either way it REPLACES that action's defaults
```

Spec grammar: optional `ctrl-` / `alt-` / `shift-` prefixes, then a single character or a
named key (`up down left right pageup pagedown home end enter esc tab backtab space
backspace delete f1..f12`). Specs are case-insensitive (`Ctrl-C` = `ctrl-c`, `K` = `k`); an
upper-case letter is spelled `shift-k`; `shift-tab` is `backtab`. `Keymap::from_config`
refuses a table with one of three errors, each printed by `lastcall tui` (exit 2, before raw
mode) and by `lastcall config` (exit 2, so a bad table is visible headlessly):

| error | message |
|---|---|
| unknown action | `[keys] unknown action \`frobnicate\` (actions: nav_up, nav_down, …)` |
| bad spec | `[keys] quit: bad key spec "hyper-q": …` |
| one key, two actions (after the merge) | `[keys] "q" is bound to both \`help\` and \`quit\`` |

The effective table is what the hint line and the help overlay show (`App.keymap`), so a
user sees their own bindings, not the defaults.

## herdr in the UI (Phase 5)

lastcall runs standalone; launched inside a herdr pane it also shows what the agents are
doing. **The socket never reaches the reducer.** A dedicated task owns the client and folds
`HerdrUpdate` values into `App` through `tui/herdr.rs`, which is the whole socket-free
vocabulary the reducer may see; `app.rs` and `render.rs` never name
`lastcall_engine::herdr`, so no `Cache`, `PaneInfo` or client handle can be reached from the
reducer (§6.6, and the gate grep below).

### The dot on a repo row

Each root's nav row can carry one glyph before its bold name, with the agent count between
the two when the root has more than one agent (`⚑3 alpha`).

| dot | glyph | style | when |
|---|---|---|---|
| ready | `⚑` U+2691 | bold unacked, dim once acked | a **ready episode** stands (see below) |
| blocked | `●` U+25CF | red | rollup `blocked`, no ready episode |
| working | `●` U+25CF | yellow | rollup `working` |
| unknown | `·` U+00B7 | dim | herdr reports a status word we do not know |
| *(none)* | | | rollup `idle`, or `done` with no ready episode |

The status shown is a **rollup**: `max` over every agent associated with the root, in the
order `blocked > done > working > idle > unknown` (`Attention` derives `Ord`, so the enum
order *is* the rule). Every dot disappears while the link is not live — a `Reconnecting` or
`standalone` lastcall claims nothing about agents rather than showing stale dots.

A **ready episode** opens the first time the rollup says `done` and closes on any non-`done`
rollup (or when the root loses its agents entirely). Inside one episode there is exactly one
alert, and an ack survives every re-derivation; a fresh `done` after the episode closed
alerts again. The ack is **local to this process** (§10, 2026-09-04): a second lastcall over
the same session derives the same flag and keeps its own ack.

A root with no pending files is still listed while it has a ready episode or a blocked
agent (`RootFlag::attention`); `working` / `idle` / `unknown` only annotate a root that is
listed for its own reasons. Such a flag-only root shows `nothing pending · agent <status>`
in both panes, so `enter` has somewhere to land — and in a nav too narrow for that line the
half that survives is `agent <status>`, because the branch line above it already says
`0 files`.

### Keys

| key | action | what it does |
|---|---|---|
| `d` | `ack` | ack the selected root's ready flag: the `⚑` dims, and the root drops out of a pending toast window. Nothing on a blocked root — there is no episode to ack — so the hint is not offered there either. |
| `g` | `jump` | `agent.focus` on the max-attention agent's pane, then `focused <agent> in herdr` in the status line. Offered for a ready **or** a blocked root: both have a pane. |
| `w` | `scope` | workspace scope on/off. Only does something when a scope was derived. |

They are ordinary `[keys]` names (`ack`, `jump`, `scope`) and rebind like any other. Both
`d` and `g` act on the **selected** root — the root of whatever the selection names.
Clicking the dot itself selects that root and acks it in one gesture (`Target::RootDot`,
the same reducer path as the key). The hint line offers `d ack` and `g jump` at tier 1 (only
while the selected root actually carries the matching flag) and `w scope` at tier 2.

### The header badge

| state | badge |
|---|---|
| connected | `herdr <version>`, dim |
| reconnecting | `herdr ⟳` |
| `mode = "off"`, or no link asked for | `standalone`, dim |
| `mode = "on"` and the link failed | `standalone: <reason>` |

A click on the badge (`Target::HeaderHerdr`) puts the full text in the status line, which is
how a truncated reason is read.

### Workspace scope

When the pane's workspace can be identified (`HERDR_WORKSPACE_ID`, read through the engine's
injected `Env` and not `std::env`), `[herdr] scope = "workspace"` hides roots outside it and
the mandatory notice says so:

```text
scope: <workspace label> · 3 repos hidden (w shows all)
```

`w` toggles it for the session; `scope = "all"` starts with it off. A hidden root is still
watched — the scope is a view, not a filter on the engine.

### Configuration

lastcall's own `config.toml`:

```toml
[herdr]
mode = "auto"      # auto | on | off — `on` makes a failed link visible in the badge
session = "work"   # optional named-session pin
toast = true       # ask herdr for a desktop notification when a repo first goes ready
scope = "workspace" # workspace | all
```

Those four are the whole table (`HerdrConfig` is `deny_unknown_fields`, so a typo is an
error, not a silent default).

**Toasts need herdr's own setting too.** In `~/.config/herdr/config.toml`:

```toml
[ui.toast]
delivery = "herdr"
```

herdr's default is `delivery = "off"`, which answers every `notification.show` with
`disabled` — so with `[herdr] toast = true` and herdr left at its default, nothing is shown
and nothing is retried. The window is deliberate rather than immediate: herdr shows its own
completion toast when an agent goes `done`, and answers `busy` while any toast is on screen,
so a call at that instant would always be refused. `TOAST_DELAY` is **7 s** — herdr's default
`[ui.toast] delay_seconds` (1 s) plus its `Finished` toast lifetime (5 s) plus a second of
margin — and one `notification.show` names every root that went ready in that window and is
still unacked (`lastcall: alpha ready for review`, or `lastcall: 3 repos ready for review`
with the names in the body; two roots sharing a basename are qualified by their parent
directory). A `busy` or `rate_limited` verdict earns exactly **one** retry 5 s later; every
other reason is dropped to the debug log. Never a third request. Raising herdr's
`delay_seconds` beyond about six seconds makes our toast lose that race for good.

### The demo (Gate 5's sponsor item)

herdr derives `done` **only for a completion in a tab the user is not viewing** (§5.7), and
focusing that tab silently flips `done → idle` with no event. So the layout matters, and a
side-by-side pane in the same tab is documented as *never flags — you watched it happen*.
The recipe below is the sponsor's real layout: the agent in a repo workspace, lastcall in the
parent-dir workspace.

1. In herdr, open a workspace on the **parent directory** of your repos and run lastcall in
   a pane there. The badge should read `herdr <version>`.
2. In a **different herdr workspace** (its own tab), open one of those repos and start an
   agent in it.
3. Switch back to the lastcall tab and leave it focused while the agent works. The repo's
   row shows a yellow `●` while it runs.
4. When the agent finishes — with the lastcall tab still the one you are looking at — the row
   flips to a bold `⚑`, the repo is listed even if it has nothing pending, and (with
   `[ui.toast] delivery = "herdr"` in herdr's config) a toast follows about seven seconds
   later.
5. `g` jumps to the agent's pane; `d` dims the flag without leaving the review.

If the flag never appears, check the tab: a completion in the tab you are *watching* is one
herdr never calls `done`.

## Hit-testing

`render` returns a `HitMap`: the nav and main inner rectangles (`pane_at`, for the wheel)
and a list of `(Rect, Target)` regions. **Regions are pushed general-to-specific and `at`
scans them last-to-first**, so a nav row wins over its pane, a hunk header wins over the diff
body, and the divider column wins over both (`render_hit_map_prefers_specific_targets`).
The loop keeps the `HitMap` of the *last drawn* frame — not `App` — and drops it on
`Resize`, so a press between a resize and the next render hits nothing rather than something
stale (`run_press_resolves_through_the_hit_map_and_resize_invalidates_it`). A press becomes
`Action::Press(col, row)`; the loop resolves it to a `Target` (`NavRoot`, `NavRow`,
`NavGroup`, `DiffHunk(i)`, `DiffBody`, `Divider`, `HeaderAcceptAll`, `FileAccept`,
`HunkAccept(i)`, `RootDot`, `HeaderHerdr`) and calls `App::hit`, which is the same reducer path the equivalent key
takes (`app_hunk_click_equals_hunk_key`, `app_accept_hunk_by_keys_equals_hunk_accept_click`).
While the confirm modal is open `hit` ignores every target, and `Ui::event` drops every
mouse event — press, drag, release, wheel — before it reaches the app at all (the wheel over
the nav otherwise calls `move_selection` directly, around `handle`'s gate); only its keys,
the `quit` keys and `Resize` get through (`run_mouse_is_dropped_under_the_modal_but_resize_passes`).

## Adding a widget, with a snapshot

1. Put the state it needs on `App` and fold it in `apply` / `handle` (return `Changed::Yes`
   only when a frame could differ; add an in-module reducer test).
2. Draw it in `render.rs` from `&App` and the area alone; push any clickable region onto the
   `HitMap` after the more general region it sits in, and give its `Target` a `hit` arm.
3. If a key reaches it, add the `Action` and its `DEFAULT_KEYMAP` entry (with `from_name` and
   `describe`); `input_every_action_is_reachable` insists on a default key, and the parity
   tests are the pattern for "the click does what the key does".
4. Add a scene to `crates/lastcall/tests/test_e2e_tui_snapshots.rs`: build a `Scene` (the
   three-root `fixture_parent`, or `Scene::clean`), feed the engine's piles into an `App`,
   drive it with `Action`s, then `snapshot("tui_<name>", &app, W, H)`. That pins two
   `insta` snapshots under `crates/lastcall/tests/snapshots/` — `*_frame` (the `TestBackend`
   symbols) and `*_styles` (the non-default style runs from `render::styles`, since the
   frame alone cannot show an inverted header or a focused border) — and asserts the frame
   is reproducible. Frames show basenames and root-relative paths only, never the temp path.

Run them with `just test-e2e` (the whole tier) or
`just cargo test -p lastcall --test test_e2e_tui_snapshots`. To accept a changed frame:
`just snapshots-update` (`INSTA_UPDATE=always`, then a proving re-run), **read the diff of
every `.snap` it rewrote** — overlapping text, a missing selection or a stale count is a
bug the snapshot would otherwise bless — and commit the files with the change that caused
them. Reading a `.snap`: the header is insta metadata; the body is the 100×30 frame (or the
scene's own size) exactly as the terminal would show it, and in a `_styles` file each line
is `<row> <from>..<to> <fg> <bg> <modifiers>` for one run of non-default cells (`2 35..37
Green Reset -` is a green `+1` on row 2).

## The PTY harness

Snapshots prove *what* a frame shows; the PTY tier proves the binary in a real
pseudo-terminal does the right thing over time. `lastcall_testkit::pty_tui` spawns any
command inside a `portable-pty` terminal (100×30 by default) and feeds a `vt100` screen
**and** a raw byte transcript from a reader thread (the parser eats escape sequences; the
off-sequence assertions read the raw log). `PtyCommand::new(bin).isolated_lastcall(home,
config, state_dir)` sets `HOME`, `LASTCALL_CONFIG`, `LASTCALL_STATE_DIR`, the null git
configs and `TERM=xterm-256color`, and strips `XDG_CONFIG_HOME`, `XDG_STATE_HOME`,
`LASTCALL_LOG*` and `HERDR_*`, so the
child never sees the real state dir. On a `PtyTui`: `wait_for(timeout, |screen| …)` polls the
screen every 10 ms and fails early if the child exits; `screen_text()` / `rows()` /
`inverse_at(row, col)` (cell attributes); `send(bytes)`, `click(col, row)` (an SGR mouse
press+release), `resize(cols, rows)` (a real SIGWINCH); `wait_eof` / `wait_exit` bounded;
`raw()` for the transcript. Children are in the testkit's PID registry: kill on drop, kill on
panic. It skips with a visible reason only when the host cannot open a PTY.

`crates/lastcall/tests/test_e2e_tui_pty.rs` runs the built binary with `tui --poll 1` over a
fresh fixture parent per scene: the first frame (`scanning 3 roots…` before any row, the
alternate screen and mouse capture on), an appended line to `alpha/f1` showing up on screen
within the live-update budget (the clock starts after the write returns; two tries, the
minimum must be ≤ 750 ms debounce + 1 s = 1.75 s, both printed), `n`/`p` with the inverted
hunk header, a click on the `beta` row changing the main header, `q` and Ctrl-C leaving the
terminal restored (mouse off in crossterm's order `?1006l ?1015l ?1003l ?1002l ?1000l`,
`?1049l`, `?25h`, no log line in the transcript), a resize below 40×10 and back, the
not-a-terminal exit, and the three `[keys]` errors exiting 2 before the alternate screen.
The scenes are serialized with a mutex; timings are printed with `stderr().write_all` so
they survive libtest's capture. Ratatui draws only the cells that changed, so a fresh
frame's words are not contiguous in the raw transcript — assert on the `vt100` screen, and
use the raw log only for escape sequences and ordering.

The two Phase 4 scenes drive the accept loop through the same binary:

- `pty_accept_loop_and_restart` — a fixture "agent" (`FixtureRepo::open_in` on the
  scene's `alpha`) writes `f1` with two hunks, `f2`, `f3` (committed) and six added files
  before the reviewer looks; the screen shows `3 repos · 12 files`; `a` on `f1`'s first
  hunk → `accepted f1 · 1 hunk left` and the row reads `+1 −1`; `A` → `accepted f1`, the
  selection lands on `f2`; `ctrl-a` → `Accept all 11 files across 3 repos?` with `1
  grouped upstream · 0 collapsed`, `y` → `nothing pending across 3 roots` and `accepted 11
  files in 3 repos`. Then the scene reads the three `ledger.json` files from the state dir
  (empty `overrides`, a moved `seen_tree`, alpha's `seen_at.head_commit` = the agent's
  commit); `q` exits 0 with the terminal restored; the agent runs `git commit -a`; a
  **second process** on the same state dir shows `scanning 3 roots…` then `watching …`
  with `nothing pending across 3 roots` and no file row ever drawn (the agent's commit
  moved HEAD, not a baseline); one more edit shows `M f2  +1 −0` and `1 repo · 1 file · 1
  hunk`.
- `pty_accept_refused_when_file_moves` — `f1`'s diff open, the agent appends a line, `A`
  goes out before the 750 ms debounce has rescanned: the status reads `f1: changed since
  rendered; not accepted`, the row stays, the ledger has no override; once the rescan
  shows `M f1  +2 −1`, `A` accepts.

Both print `PTY accept …` timing lines. The status bar is asserted as `<text> · <age>`
exactly, so `accepted f1` cannot pass for `accepted f1 · 1 hunk left`.

## Probes and logging

- `just probe-tui` — release build, a fixture parent in `/tmp/lc-probe-<pid>/`, then the
  interactive TUI over it with `--poll 1`; prints the `LASTCALL_CONFIG` / `LASTCALL_STATE_DIR`
  lines first so you can re-run by hand and edit a fixture file from another terminal. The
  accept keys work in it (its banner says so): review the fixture to zero with `a` / `A` /
  `ctrl-a`, `q`, re-run the printed command with the same two exports — the relaunch shows
  `nothing pending across 3 roots`.
- The sponsor's demo on a real working directory, without touching the real state dir:
  `LASTCALL_STATE_DIR=$(mktemp -d) lastcall` in a parent where an agent has been working;
  first sight makes everything already there "seen", so edit or let the agent edit, review
  to zero, `q`, relaunch with the same `LASTCALL_STATE_DIR` → zero. The kickoff's
  `pty_accept_loop_and_restart` is this recipe under the harness.
- `just bench` — the performance baseline on the release build (`docs/dev/bench.md`).
- `just probe-tui-screen` — the transcript form: the PTY harness drives the release binary
  over the same fixture, appends a line to `alpha/f1`, waits for the row's counts to change,
  opens the diff and prints the screen as text plus the exit code after `q`. About three
  seconds; nothing is left behind.
- `LASTCALL_LOG_FILE=/path/to/log lastcall` — `tracing` never writes to the terminal while
  the screen is up; with this variable set the TUI appends to that file, filtered by
  `LASTCALL_LOG` (an `EnvFilter` directive, default `info`). Unset, there is no subscriber at
  all. These two reads are the only environment access under `tui/`; everything else comes
  through the engine's `Env`.

### What `debug` says (Phase 6 deliverable 8)

```sh
LASTCALL_LOG=debug LASTCALL_LOG_FILE=/tmp/lc.log lastcall tui
```

Answering "why is it slow / why did nothing happen" without a debugger. Seven messages, and
the **field names are the interface** — greps and the capture test
(`crates/lastcall-engine/tests/test_integration_tracing.rs`) depend on them, so rename one
and fix both:

| message | fields | where |
|---|---|---|
| `open done` | `roots`, `ms` | `Engine::open`, once per process |
| `scan done` | `root`, `ms`, `rows`, `seq` | every scan, `Engine::scan` and each root of `scan_all` |
| `watch event` | `path`, `kind`, `scheduled` | one per actionable filesystem event |
| `scan due` | `root`, `reason` | a root's scan was scheduled |
| `head inspect` | `root`, `changed` | every HEAD inspection, poll or event |
| `fold` | `source`, `changed` | one per event folded into a `Pass` |
| `draw` | `cause`, `ms` | one per frame actually drawn |

Two closed vocabularies:

- `scheduled=` is what the watcher routed the event to: `scan` (a worktree path), `head` (a
  git-dir path on the allowlist), `ignore` (filtered out) — `Scheduled::label`.
- `reason=` is why a scan became due: `event` (a filesystem event), `head` (HEAD moved and
  the inspection scanned), `rescan` (the backstop, a watcher error, or the root set
  changing), `refresh` (the initial pass and the post-install catch-up).

`fold`'s `source=` and `draw`'s `cause=` share one vocabulary — `input`, `engine`, `local`,
`herdr`, `timer`, `tick` — so `rg 'draw' /tmp/lc.log` counts frames and says what caused
each, and the `fold` lines between two `draw` lines are exactly what that pass coalesced.
`watch installed` (with `roots`) and `rescan backstop` mark the two lifecycle moments.
There is no `set_global_default` anywhere in the tree, in tests included: the capture test
is its own integration binary because `tracing` caches callsite interest **globally**, so a
sibling lib test scanning on a subscriber-free thread would poison it.

## Gate greps

```sh
rg -n 'Command::new\("git"\)' crates                       # engine git.rs, plus the testkit's fixture builder; nothing under tui/
rg -n 'std::env::var|home_dir\(' crates/lastcall/src        # the two LASTCALL_LOG* reads in tui/term.rs, plus LASTCALL_PARALLELISM in commands/mod.rs (test-only override, never under tui/)
rg -n 'lock\(' crates/lastcall/src/tui                      # nothing
rg -n 'Rendered::of' crates/lastcall/src                    # only tui/app.rs (requests come from the held rows)
rg -n 'last_pile|scan_all\(|\.scan\(' crates/lastcall/src/tui/app.rs   # nothing (the reducer never scans)
rg -n 'println!|eprintln!|print!' crates/lastcall/src/tui   # nothing (the messages are in commands/)
rg -n 'thread::sleep|tokio::time::sleep' crates/lastcall/src/tui   # nothing
rg -n 'lastcall_engine::herdr' crates/lastcall/src/tui     # only tui/herdr.rs and tui/run.rs (the task side); never app.rs or render.rs
rg -n 'e\.(restore|flag|unflag)\(' crates/lastcall/src     # only tui/run.rs (restore and flag reach the engine through one seam)
rg -n 'OpenOptions|File::create|fs::write' crates/lastcall/src   # tui/term.rs (the log file) and tui/run.rs (the export fallback); no worktree file is ever opened for writing
cargo tree -e normal -p lastcall -p lastcall-engine | grep -c testkit   # 0
```
