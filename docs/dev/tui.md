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
  and the notice do not both fit — 60 columns — the control is dropped, then the herdr
  badge, since `^A` duplicates the control and nothing else says what is watched or
  whether herdr is answering; a notice that cannot fit beside the counts at all is
  dropped instead, and the badge and the control return), the body — a nav pane
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
`watch` does, enters the terminal, seeds the app with the engine's roots (`sync_roots`),
starts the **launch hold** (`App::start_loading`) and the status `scanning N roots…`, and
draws the hold's pane — the first piles arrive through the watcher a moment later (about
1.6 s on the fixture under the PTY harness), and the listing lands as one frame when the
last root has reported. `--poll N`
shortens the HEAD-poll and rescan backstops exactly as for `watch` (`just probe-tui` uses
`--poll 1`, the deterministic setting on a host whose FSEvents are unreliable).

#### The launch hold (Gate 8 sponsor run, 2026-09-07)

The engine's initial pass used to scan the first root on its own so its pile reached the
screen early (Phase 5 deliverable 1b); the sponsor's recording showed the cost — one repo
listed for ~100 ms as if it were the only one with changes, then the full list — and ruled
it out: "discovered N roots, checking status…" and nothing listed until every root has
reported, with per-repo progress so one huge repo holding things up is visible, and
numbers only once a load has run longer than a second. The rules as built (`App::Loading`,
`render_main`'s `None` arm, `Loading::COUNTER_AFTER` = 1 s):

- **From `sync_roots` until every root has reported, `App::is_listed` is false** for every
  root, and the right pane reads `discovered N repos, checking status…` with one line per
  root (`  <name>  <branch>`) — the same lines as the empty state, so the frame does not
  jump when the hold ends. **repos** is the user-facing noun wherever a count is shown
  here and in the empty states; `root` stays the config and CLI word (Design pass D3,
  ruling R5). The header agrees with the pane rather than contradicting it: while the hold
  is on it reads `lastcall  N repos · checking status…` — the one count it knows and none
  of the ones it does not — with `[Accept All]` dim, as it is whenever nothing is listed.
  The **status line is not** set to a second sentence about the same wait: `run.rs` writes
  no `scanning N roots…`, the pane is the hold's home and its only clock, and the bottom
  row shows the hints exactly as the empty state does until the engine's own `watching …`
  notice lands.
- **A root "reports" three ways:** the watcher's `EngineEvent::Scanned { root, rows }`
  (sent from the pool thread the moment that root's scan returns, before the batch's
  piles land — `Engine::scan_all_with`'s hook, `try_send` from under the engine lock so a
  full channel drops a tick rather than parking a worker behind a lock the consumer may
  be waiting on), its `Pile`, or a `scan failed` notice. A **global** notice
  (`watching …`, `watch installation failed`) ends the hold outright — every such notice
  comes after the initial scans, so whatever has not reported never will. No roots → no
  hold.
- **The first second is a static line.** Below `COUNTER_AFTER` there are no digits: a
  fast launch shows one calm frame, not a flash of `loading… 23423423432`. From one
  second on (`Loading::counting`, measured on the app clock, so the `Tick` action redraws
  while the hold is on) the pane adds a dim
  `K of N checked · F files pending so far · Ss` and a `✓` in the **leading column** of
  each root that has reported (Design pass D4, ruling R6: the tick takes the row's own
  two-space indent — `✓ alpha  main` / `  beta  main` — so the ticks line up whatever the
  branch labels are and the slow repo is the one gap in the column, a glance rather than a
  read). `F` is the sum of the reported roots' pending rows; there is no intra-repo progress (the time is inside
  `git`), and the seconds counter is the liveness signal.
- **The solo first-root scan is gone** from `watcher::run_loop`: every root goes through
  the one `scan_all` on the pool, and the piles land together in path order with
  `scan_seq` numbered that way. `bench.md` S1's `first_pile_ms` changed meaning with it
  (see the note there). The PTY harness pins the order: `discovered 3 repos, checking
  status…` before the first row, and `nothing pending across 3 repos` never before it
  (`wait_first_piles`); `watch --json` prints the tick as
  `{"event":"scanned","root":…,"rows":N}` and is otherwise unchanged.

**Seeing the hold slowly.** On a developer's checkout the hold is over in a second or two
and the counter never shows; the design is for the user with hundreds of repos or a slow
disk, and the way to look at it as they will is `just probe-tui-slow`, or
`PATH=$PWD/scripts/slowgit:$PATH SLOWGIT_SLOW_REPO=<name> lastcall` over any parent dir.
`scripts/slowgit/git` is a `git` that sleeps before the two subcommands only the scan runs
(`diff-files`, `ls-files` — `SLOWGIT_MS` per call, default 800 ms, four calls per root)
and then execs the real one, so discovery still runs at full speed and only the
"checking status" phase stretches; one root named by `SLOWGIT_SLOW_REPO` gets
`SLOWGIT_SLOW_MS` (default 2,500 ms) per call and is the last gap in the ✓ column. The sponsor
approved the hold on exactly this view (§10 2026-09-07 (v)): a 20-root workspace held for
~11 s with the counter ticking, the ✓s filling in, and the slow repo visibly the one
holding things up. Any UX change to the hold should be looked at both ways — fast, where
the rule is "one calm frame, no digits", and slow, where the rule is "you can see who is
holding things up". The first thing the slow view found was a launch race the fast view
had always hidden: `run::run` read the root list through the engine lock *after*
`engine.run`, and the watcher's initial `scan_all` takes that lock the moment it starts,
so whichever got there first won — on a fast checkout the TUI, on the stretched scan the
watcher, and the `discovering roots…` line then stood for the whole scan with no hold at
all. The roots are now read while the engine is still owned, before the watcher exists
(`root_metas(&engine)`), so the first frame never waits on a scan.

**One known cost sits in front of all of that.** `term::enter()` asks the terminal whether it
speaks the kitty keyboard protocol (`CSI ? u`, then `CSI c`) before the input thread starts,
and a terminal that answers neither costs crossterm's full **2 s timeout** — once per
process, before the first frame — the answer is a `OnceLock`, so an `$EDITOR` resume
re-pushes the flags without re-asking and pays nothing (F8, F18).
`LASTCALL_KEYBOARD=plain` skips it entirely. The full entry, with the
terminals known to answer and the one-line flip to a `TERM` allowlist, is
[`bench.md` "Known costs"](bench.md#known-costs).

#### The first-launch tour (Phase 10)

`tui/tour.rs` holds everything about the welcome except the painting: the marker on disk,
the `Plan` the loop drives it with, the cards and their wording, the fixed keys, and each
card's lines as text. `render::render_tour` paints it, beside every other modal's painter,
because a hit map is a fact about a frame and not about a card.

The split follows the reducer's own rule. Every file this feature touches is touched from
the loop: `commands/tui.rs` builds `tour::Plan::new(force, env, state_dir)` **before** the
terminal is taken (one `stat` of `<state_dir>/first-launch.json`, and at most one small read
of it), and the loop then asks `Plan::due(&app)` before each draw, calls `Plan::open` to
parse the config document and decide the cards, calls `Plan::write` when a choice asks for
it, and calls `Plan::dismissed` when the overlay closes. `App` itself only holds
`Option<Tour>`: which cards, which one is on screen, which row, and the sentence a failed
write left behind.

`Plan::due` is four conditions, and each is there for a reason a fast checkout will not show
you:

- `app.loading.is_none()` — **not during the launch hold.** The hold is the frame that says
  what lastcall is doing; an overlay over it would be a second thing to read about a wait.
- `!app.herdr.scope_pending` — and not before the scope verdict either, for the same reason
  nothing is listed before it: the herdr card's own condition asks what the scope came out
  as.
- `app.size >= (60, 14)` (`MIN_COLS`, `MIN_ROWS`) — **never on a screen too small to read
  it**, and in that case nothing is written, so the welcome is still owed on the first launch
  that has room. The unit test `tour_every_card_fits_the_smallest_frame_it_opens_on` walks
  every card at exactly that size.
- `app.tour.is_none()` — it does not open over itself.

Three cards at most, decided once when the overlay opens. `Card::Keys` is always there.
`Card::Herdr` needs all four parts of its condition — a live link, a scope derived, the scope
honoured, and a config file that has never said `scope` under `[herdr]` — because any one of
them missing makes the card an offer about nothing. `Card::Empty` needs ten listed
repositories with nothing pending (`EMPTY_CARD_MIN`), `t` off, and no `hide_empty_repos` in
the file.

**Keys.** `tour::tour_action` resolves *before* the keymap and before every other modal
(`run.rs`'s event arm), so while the overlay is up nothing else in the program sees a key.
`enter` applies the selected row or advances, the arrows and `j`/`k` move between a choice
card's two rows, `esc` and `q` skip the rest. `q` never quits here, which is the note modal's
rule and for the note modal's reason: a highlighted row is a question, and the reader has to
be able to answer it without leaving. A quit bound to some other printable key skips too;
only the non-printable spellings (`ctrl-c` out of the box) still quit, and that quit writes
the marker on its way out — `Plan::owed()` is asked after the event loop ends.

**The one config write in the program.** The second row of a choice card is the only thing
in lastcall that edits `config.toml`, and it does it through the engine's
`config::write::Document`, which is `toml_edit` and therefore format-preserving: your
comments, your key order and your blank lines survive. The row applies its setting to the
live `App` first and asks for the write second, so a write that fails still leaves the change
in force for the session — and the card keeps its place, with the footer replaced by what
went wrong and the TOML line to add by hand. When there is no config file at all the write
creates one holding a dated `# written by lastcall's first-launch tour on <date>` comment and
just that setting.

**The marker.** `<state_dir>/first-launch.json` is `{"shown_at":<unix secs>,"version":"…"}`,
written with `write_stamp`'s idiom (temp beside the target, then rename) by *any* dismissal:
finishing, skipping, or quitting with it open. Absent, unreadable and unparsable all mean
"not shown", because a record we wrote and cannot read is not worth refusing to help a new
user over. `lastcall tui --tour` ignores it for that run and rewrites it on dismissal, and
the help overlay's last footer row (`render::TOUR_NOTE`) is where a reader finds that out.

**Scenes.** Three snapshot scenes, each at 100×30 and 80×24: `tui_tour_keys`,
`tui_tour_herdr`, `tui_tour_empty`. Six PTY scenes. `tui_tour_first_launch` is the one a new
user actually gets: no config file at all, fourteen repositories of which twelve are quiet,
the keys card, the empty card's live change from `14 repos` to `2 repos`, the created file's
exact bytes, and then a second launch over the same state directory that shows no welcome
and keeps the setting. `tui_tour_first_launch_herdr` is its sibling under a live workspace
link, where the herdr card is the one that writes.
`tui_tour_preserves_config` (a one-line diff on a hand-written file that already has three
tables, including a `[keys]` table, with the new key landing at root level above the first
table header), `tui_tour_skip`, `tui_tour_flag` and `tui_tour_quit_writes_marker` are the
rest. The harness knows the marker by name: `pty_tui::MARKER_FILE` and `MARKER_SEEN` are
what `isolated_lastcall` drops into the state directory so every *other* scene launches
without the welcome, `.tour(true)` removes it again, and `.keep_marker(true)` leaves
whatever is on disk alone, which is the only way a second launch can see what the first
one wrote. `tour_marker_is_the_file_the_harness_writes` pins the two spellings together,
because the testkit sits below `lastcall` in the dependency graph and cannot import the
constant.

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
- **A restore asks whenever it removes something.** A hunk restore starts immediately; a
  file restore opens the confirm modal first. The CAS is the guard either way and the
  content a restore drops stays addressable in the private store, but a whole file going
  back is the bigger surprise. The one row where `u` is *not* a hunk restore is an **added**
  file whose whole content is one hunk: restoring that hunk is the engine's removal path
  (`ops_restore_hunk_on_an_added_file_removes_it`), so `restore_scope` returns the file
  scope and the delete question opens (verifier (b) F3 — it used to delete the file with no
  question and then report `restored f1 hunk 1` about a path that was gone). An added row
  with several content hunks keeps the hunk scope: there a hunk restore really is partial.
  The
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
| `Enter` | send: `Effect::Flag`, the modal closes (an empty note is allowed: the flag is the message) |
| `Ctrl-J` | newline (works in every terminal) |
| `Alt-Enter` | newline, where the terminal reports Alt (Option-as-Meta) |
| `Ctrl-Enter` | newline, where the terminal can tell it from `Enter` — the same kitty-protocol condition as `Shift-Enter`; everywhere else those bytes *are* `Enter` and send |
| `Shift-Enter` | newline **only** under the kitty keyboard protocol — see below |
| `←` `→` `↑` `↓`, `Home`/`End`, `PgUp`/`PgDn` | move the caret (`TextBuf::apply`) |
| `Ctrl-A` / `Ctrl-E` | line start / line end |
| `Alt-←` / `Alt-→` (or `Ctrl-`) | word left / word right |
| `Backspace`, `Delete` | delete around the caret |
| `Ctrl-H` | Backspace: crossterm reports the byte `0x08` as ctrl-h (only `0x7f` is `Backspace`), so a terminal set to send `^H` for its Backspace key keeps the key |
| `Alt-Backspace` / `Ctrl-W` | delete the word before the caret |
| `Ctrl-K` | delete to the end of the line |
| `Tab` | inserts a tab character |
| `Esc` | cancel — nothing is written |
| the `quit` binding, non-printable only | quit (`Ctrl-C` by default): a modal is never a trap |
| anything else | swallowed |

The buffer gets first refusal, so **a `[keys] quit` bound to `ctrl-a`, `ctrl-e`, `ctrl-h`,
`ctrl-j`, `ctrl-k` or `ctrl-w` is typed or moved as an edit inside the buffer, not obeyed**
(verifier (a) F7). That is the intended direction — a note is text, and losing a line to a
rebound quit is worse than needing `Ctrl-C` — but it is the reason to keep `quit` on a key
the buffer has no use for. `Ctrl-U` is swallowed with no edit at all.


The modal edits a [`TextBuf`](../../crates/lastcall/src/tui/textbuf.rs), the same buffer the
inline editor uses, so what is typed round-trips byte for byte.

#### `Shift-Enter`, and which terminals can report it

Without the kitty keyboard protocol `Shift-Enter` is **byte-identical to `Enter`**: the
terminal sends `\r` either way, so a modal that treated it as a newline would send the note
instead. Phase 8 asks for the protocol rather than guessing (ruling P9): `term::enter()`
calls crossterm's `supports_keyboard_enhancement()` **once per process**, before the input
thread starts, and pushes `DISAMBIGUATE_ESCAPE_CODES` when the answer is `Ok(true)`;
`term::restore()` pops the flags before leaving the alternate screen on every exit path. An
`Err` — including the 2 s timeout against a terminal that never answers — is "off".

The key line says which world it is in, and that line is a promise: `⏎ send   ^J newline
Esc cancel` when the protocol is off, `⏎ send   ⇧⏎ / ^J newline   Esc cancel` when it is on.
The help overlay carries the same promise in one row (`render::newline_note`).

Terminals that report the protocol (so `⇧⏎` works there):

| reports it | does not |
|---|---|
| kitty, WezTerm, foot, Ghostty | Terminal.app |
| iTerm2 ≥ 3.5 with the option enabled | tmux without `extended-keys` |
| | herdr's terminal, as of the Gate 7 run |

`LASTCALL_KEYBOARD=plain` skips the probe altogether — an environment switch, not a
`[config]` key. The PTY harness sets it in `PtyCommand::isolated_lastcall` so no scene pays
the 2 s timeout; `pty_keyboard_enhancement_probe_is_answered_and_swallowed` unsets it and
plays a kitty-protocol terminal to prove the query is written, the answer is believed, the
flags are pushed and popped, and no byte of the reply ever reaches the app as a key.

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
to press Enter. The payload ends with `STAGE_TAIL` (a newline and a blank line) **inside**
the markers: the closing fence gets its own line and the next flag staged into the same
buffer starts a block of its own. The sponsor's Gate 7 run found three flags running together
— each closing fence followed on the same line by the next `lastcall flag ·` header, which a
Markdown reader nests inside the first code block — because the first design left the
newline out for fear of submitting. Inside bracketed paste a newline is text; and the diff
already carries dozens, so an application that ignored the markers would have submitted long
before the tail. The real-pane proof (`herdr_real_send_text_lands_unsubmitted`) stages the
tail with the rest and shows nothing runs until Enter. A send that
fails is a status line and nothing more (`flagged f1 · send failed: <reason>`): the flag is
in the ledger either way, which is why the flag's own label travels with the send —
`Effect::Stage { flag, .. }` → `Local::Staged { flag, .. }` — rather than being read back
off `App` when the answer lands.

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

**Every answer carries its own kind and label.** Two flag writes can be in flight at once —
`m` again while a send is still out, or `m` then `shift-m` on the same row, whose two
blocking tasks the engine's mutex does not order — so `Local::Flagged` carries `FlagKind`
(`Flag { label }` or `Unflag`), built from the effect the loop dispatched, and `Staged` and
`Exported` carry the flag's words with them. Nothing about a flag is read back off a slot on
`App` when its answer lands: an unflag is never mistaken for a cancelled send (which would
append a blank entry to the day's export file), and a second flag is never reported as
`flags cleared` and then dropped. An empty export is never sent by any route.

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

## Editing (Phase 8)

Phase 8 adds the fourth answer: change it. Three keys and one shared buffer.

| key | what it is |
|---|---|
| `i` (`edit`) | the **inline editor**: the file replaces the diff pane, `Ctrl-S` saves it through the engine's CAS |
| `shift-i` (`edit_external`) | suspend lastcall, hand the terminal to `$VISUAL`/`$EDITOR` at the same line, and ask about what came back |
| `v` / `y` (`select` / `copy`) | select diff lines and put them on the clipboard over OSC 52 |

Both editors open on the **same line**: `App::edit_hunk` picks the content hunk under the
diff cursor when the diff has focus and the row's first content hunk otherwise, and
`Hunk::editor_line()` turns it into a one-based line past the hunk's leading context. The
synthetic mode hunk is never it — there is no text in it — and a row with no content hunk at
all opens at line 1. `i` and `shift-i` share the function, which is what keeps them from
landing on two different hunks of one row; the rule is pinned once per key —
`app_edit_external_from_the_nav_uses_the_first_hunk_line` (nav → first hunk, diff → the hunk
under the cursor, a collapsed row → line 1) and
`app_edit_opens_at_the_hunk_line_and_marks_its_lines`.

`App::edit_target` refuses before either key does anything: a deleted row, a mode that is
not `Regular`/`Executable` (a symlink, a fifo), or a path that is not UTF-8 — status
`not editable` (`app::NOT_EDITABLE`), one sentence, because there is nothing behind it. The
inline editor's *other* refusals come from the engine and say more (below).

**Both keys are swallowed while a modal is open.** `Ui::event` routes note → picker →
confirm → editor → keymap, so `i`, `shift-i`, `v` and `y` are the modal's text or nothing
while a question is on screen (`app_edit_and_select_are_swallowed_while_a_modal_is_open`).

### The text buffer

`tui/textbuf.rs` is one editable buffer with two consumers — the note modal and the inline
editor — and one rule that governs the file:

> **`TextBuf::from(t).text() == t` for every UTF-8 `t`.**

A review tool that edits a file hands back exactly what it was given plus the user's change
and nothing else, so: line **endings live beside the text** (`Line::end`, `Ending::{Lf,CrLf}`),
a file with no trailing newline keeps not having one (`last_terminated`), a lone `\r` inside
a line is an ordinary character, and a BOM is char 0 of line 0 at zero columns. The proptest
`textbuf_round_trips_any_utf8_text` is the statement of the rule; every operation is written so
it holds afterwards (design review F6).

- **Columns.** `cursor.col` is a **char index**, so no edit can land inside a code point.
  The screen measures something else: `col_width` walks with `unicode-width` and a `\t`
  advances to the next multiple of `TAB_STOP` = 8, which is what `cat`, `less` and git's own
  diff agree on, so a file looks the same in the editor and in the diff pane beside it.
  `want_col` — the sticky column that survives a vertical move over a short line — is a
  *display* column, because that is what the eye tracks
  (`textbuf_wide_and_combining_chars_keep_columns_honest`).
- **`top` means what the wrap mode says.** `Wrap::None` (the inline editor): `top` is the
  first logical line and `left` scrolls sideways. `Wrap::Soft` (the note modal): long lines
  break for display only, `top` is the first display row and `left` stays 0. A buffer is
  rendered in one mode for its whole life, so the two readings never meet on one value.
- **A terminated buffer's final newline is not editable** (verifier (a) F9). `from("\n")` is
  one line with `last_terminated = true`, the cursor cannot pass the end of the last line,
  and no `Delete` inside lastcall can strip a file's trailing newline. That is vim's `eol`
  semantics, it is consistent both ways (`""` plus a `Newline` is two unterminated lines
  that also round-trip to `"\n"`), and `shift-i` is the way out of it.
- **A pasted `\r\n` takes the buffer's dominant ending** (verifier (a) F3): `CrLf` only when
  the buffer already uses it, so pasting Windows text into an LF file does not sprinkle CRs
  through it (`textbuf_paste_of_crlf_takes_the_buffers_dominant_ending`).
- **`join_up` keeps the lower line's ending** (verifier (a) F8): backspacing at column 0
  merges the text upward, and the surviving line ends the way the line that swallowed the
  other one did.

`input::edit_key` is the buffer's key map, shared by both consumers and by nothing else:

| key | edit |
|---|---|
| any printable, `Tab` | `Insert` (a tab is a real `\t`) |
| a bracketed paste | one `Insert` of the whole payload, newlines and all |
| `←` `→` `↑` `↓`, `Home`/`End`, `PgUp`/`PgDn` | move the caret |
| `Ctrl-A` / `Ctrl-E` | line start / line end |
| `Alt-←`/`Alt-→` (or the ctrl forms) | word left / word right |
| `Backspace`, `Delete` | delete around the caret |
| `Ctrl-H` | `Backspace` — crossterm reports the byte `0x08` as ctrl-h, only `0x7f` is `Backspace` (verifier (a) F4) |
| `Alt-Backspace` / `Ctrl-W` | delete the word before the caret |
| `Ctrl-K` | delete to end of line |
| `Ctrl-J`, `Alt-Enter` | newline, always |
| `Ctrl-Enter`, `Shift-Enter` | newline **only** under the kitty keyboard protocol (verifier (a) F5) — see "`Shift-Enter`, and which terminals can report it" above |

`Enter` and `Esc` are deliberately **not** in the table: what they mean depends on who holds
the buffer. The note modal sends and cancels; the inline editor breaks the line and closes.
Each caller checks its own two keys and asks `edit_key` second — which is also why the
buffer gets first refusal on ctrl-letters, and why a `[keys] quit` bound to one of them is
typed rather than obeyed (verifier (a) F7).

### The inline editor (`i`)

`i` does not open the editor: it asks the loop for the file's bytes (`Effect::EditInline`),
which `Engine::read_rendered` answers off the UI task. The **marks and the band are computed
in the reducer, before the read** — from the pile that is on screen — so the editor that
opens describes the file the reader was looking at, and a pile landing while the read is in
flight cannot renumber them.

The engine's refusals arrive as `App::edit_read`:

- `Refused::NotEditable` (binary, over `collapse_size_bytes`) prints
  `use shift-i: <why>` — "no" is only half an answer when there is a second way in, and
  `$EDITOR` never loads the file into lastcall so neither limit applies to it.
- everything else is the CAS speaking, in the vocabulary every other refused op uses.

**Layout** (`render::render_editor`). The editor replaces the diff pane; the nav stays.
Header: `editing <path> · line N/M[ · unsaved]`, **bold — or red while `ed.alarm`**, which
is set by a refused save and cleared by the next key. Only the **path** gives, and it gives
from the head (`render::ellipsize_head`, Design pass D9 / ruling R10): the budget is the
width less `editing `, ` · line N/M` and ` · unsaved` — the last reserved whether or not the
buffer is dirty, so the header does not shift under the reader on the first keystroke — and
the cut lands on the leftmost `/` whose remainder fits, so what is left still reads as a
path (`editing …lastcall/src/tui/render.rs · line 21/62 · unsaved`). The **basename is the
floor**: a width with no room even for `…<basename>` keeps the whole basename and lets the
row's own clip take the overflow, because half a file name answers nothing. Body: a
five-column gutter (`EDITOR_GUTTER`), then the file, no wrapping. The gutter carries the
line number, and `▎` (`EDITOR_MARK`) on **every line inside any pending hunk of the row** —
so the reader can see the rest of the agent's work while they type in one part of it. The
hunk they *entered* is tinted whole (`EDITOR_BAND_BG`, indexed 236) with its marks bold; the
caret's line is tinted brighter (`EDITOR_CURSOR_BG`, 238). A row that runs off the right edge
ends in a dim `→` (`EDITOR_CLIPPED`) — the editor does not wrap, and the rest of the line is
one `End` away. The caret is drawn **reversed**, not left to the terminal's own cursor,
because the frame is the only thing a snapshot and a PTY scene can see. The hint line becomes
exactly `^S save   Esc close` (`render::EDITOR_HINTS`).

Marks and the band follow the typing (`Editor::shift`): a mark strictly *below* an edit
moves with it, a mark on the edited line stays, and the band's end moves on `>=` rather than
`>` — splitting the band's last line leaves both halves inside the hunk the reader entered,
so the tint **grows**. The marks are never a second source of truth about the file; the next
scan's hunks are.

Nothing in the renderer scrolls. `App::clamp_editor` runs after every key and every resize
and calls `TextBuf::viewport(page_rows, editor_cols, Wrap::None)`; `render_editor` draws the
window as it stands, so the frame never depends on when it was drawn (F19). `editor_cols` is
computed from `App::size` and the renderer lays out from the frame's area — the loop feeds
both from one resize event, so they are the same rectangle.

**The mouse is here too**, because the editor is the one place a click means "put the caret
there": `EditorKey::Click(dy, dx)` is pane-relative with the gutter already subtracted and
the reducer adds the buffer's own scroll; the wheel is `EditorKey::Scroll(±n)`.

**Save answers** (`App::saved`). `Ctrl-S` sends `Effect::Save`; a second `Ctrl-S` while one
is in flight is ignored (`Editor::saving`), so one buffer is never written twice at once.

| answer | what happens |
|---|---|
| clean save | the row is **gone** from the pile that comes back, the editor closes, the §6.7 advance moves the selection off the row exactly as an accept would, status `saved <path>` |
| `Refused::Moved` | the buffer **stays**, the header goes red, status `<path>: changed since you opened it; not saved — Esc, then i to reload` |
| any other refusal | the buffer stays, the engine's own sentence on the status line |
| `LedgerBusy` / an error | the buffer stays, `ledger busy in <root> — try again` or `<root>: <e>` |

Every non-clean answer keeps the text. An agent writing the file while the reader was typing
is the one case where throwing their buffer away would be the worst possible reading of "not
saved" (F17).

`Esc` on a clean buffer closes it; on a dirty one it asks — a confirm reading
`Discard changes to <path>?` — because the buffer is the only copy.

### `shift-i`, and the suspend/resume sequence

`editor.rs` resolves `$VISUAL`, else `$EDITOR`, else `vi` (POSIX guarantees it). A variable
that is *set but blank* is an error rather than a fall-through — `VISUAL=` in a profile is a
mistake worth naming. **No shell**: the value is split on ASCII whitespace and the pieces are
argv, so `EDITOR='code --wait'` works and quotes and `$` are ordinary characters. Running an
inherited environment variable through a shell would make a review tool spawn arbitrary shell
code; the wrapper-script case is served by pointing the variable at the script.

The line flag comes from a **basename** table, so `/usr/local/bin/nvim` and `nvim` take the
same flags: `+<line> <file>` (vi, vim, nvim, view, nano, micro, emacs, emacsclient, kak),
`<file>:<line>` (hx), `<file>:<line> --wait` (subl, zed), `--goto <file>:<line> --wait`
(code, codium). Anything else opens with the file alone and the status says
`opened in <name> (no line flag known)`. **Non-waiting editors return before their save** —
`code` without `--wait`, `emacsclient -n`, anything through `open`: the child exits at once,
the file is unchanged when lastcall looks, the status says `no change`, and the user's later
save arrives as an ordinary pending row.

Before the spawn, the loop takes the same **live CAS** an accept would: if the file is not
the one the row describes, it says `<path>: changed since rendered; not opened` and opens
nothing. Talking someone into saving over an agent's newer work is exactly what a review tool
must not do.

Then `run::Suspend::run`. **Every step is load-bearing and the order is the whole point**
(deliverable 7; design review F4 and F8):

1. **Stop and join the reader thread.** Told-to-stop is not enough: a thread still inside
   `crossterm::event::read()` competes with the editor for the same tty and splits the
   keystrokes between them. It polls at `INPUT_POLL`, so the join costs at most that. (This
   is the one place the reader thread is ever joined — the quit path deliberately does not.)
2. **`term::restore()`**, which pops the keyboard-enhancement flags *before* leaving the
   alternate screen, so the editor starts on a terminal reporting keys the way its own reader
   expects.
3. **Spawn the child** with the three standard descriptors inherited and cwd at the root, and
   wait. Its signal dispositions are the shell's own: `exec` resets every *handler* to
   `SIG_DFL`, and lastcall installs handlers (tokio's) rather than `SIG_IGN`, so nothing this
   process did is inherited and a `^C` at the editor interrupts the **editor**.
4. **`Signals::resume`** before anything is drawn: that same `^C` was delivered to lastcall
   too, and the loop must not read it as "quit" the moment it runs again. For
   `EDITOR_SETTLE` = 50 ms after the resume an interrupt is ignored — a real `ctrl-c` a
   moment later still quits (`pty_editor_ctrl_c_does_not_quit_lastcall`, which now **ends**
   with that second `^C` 300 ms after the resume rather than with a `q` — verifier (b) F3).
   **Residual (verifier (b) F5):** `Signals::register` also
   registers `SIGQUIT` and drops the stream on the spot, and tokio's handler stays installed
   for the life of the process — so `kill -QUIT <lastcall>` from another terminal does
   nothing at all, at any time, not just during a suspend. That is deliberate (it is what
   keeps a `^\` at the cooked editor from dumping core behind it, and `sigaction` is
   `unsafe`), but a lastcall that ignores SIGQUIT is not hung: `kill -TERM` and `kill -INT`
   both quit through the restore path.
5. **`term::enter()`, then replace the guard without dropping it.** The old guard's `Drop`
   calls `restore()`, which would now undo the *live* terminal it never owned, so it is
   `mem::forget`ten rather than dropped. Then **one `ESC [ 6 n`**, fire-and-forget
   (`run::nudge_the_tty`, verifier (b) F1). A key typed in this window used to sit in the tty
   until the *next* key, which delivered both at once — a `q` or a `y` that looked like it
   did nothing: crossterm registers the tty with kqueue once per process and edge-triggered
   (`EV_CLEAR`), and xnu's `TIOCSETA` moves the pending cooked line into the raw queue
   without `ttwakeup`, so nothing fires for a byte that is already readable. The terminal's
   reply is an edge the kqueue does fire on, and the read it wakes drains the stuck byte with
   it; the reply itself is parsed by crossterm as an internal `CursorPosition` that `read()`
   never surfaces. **Residual:** on a terminal that does not answer DSR nothing changes —
   the key is still delivered with the next one. `pty_editor_key_typed_during_the_editor_is_not_stuck`
   proves the fixed path with the harness playing a terminal that answers
   (`PtyCommand::answer_cursor_position`, `testing.md`).
6. **A fresh channel and a fresh reader thread.** The old channel can still hold the key
   release of `shift-i`, or a `Resize` the editor caused — neither means anything to the
   resumed TUI.
7. **A synthetic `Resize` to the size the terminal has now.** Through `Terminal::resize`,
   **not** `Terminal::clear`: clear snapshots the cursor with a DSR *query*, which needs a
   reply from a terminal the freshly spawned reader is now polling, and a terminal that never
   answers turns that into crossterm's two-second timeout and then an `Err` that would be
   fatal here. `resize` clears the screen and resets the back buffer and asks the terminal
   nothing.

A spawn that never started (`editor not found: <program>`, the verb first because the status
row ellipsizes from the tail and `$EDITOR` is often an absolute path) is a status line and
nothing else. A child that ran — whatever its exit status; an editor that quits with an error
still wrote, or did not — produces `Effect::EditorReturned`, and the answer table is in
[`engine.md`](engine.md) under "The blessing on `$EDITOR` return": `no change`, one of the
four `left pending` sentences, or the confirm
`<path> edited — mark every hunk in it reviewed?` (`y`/`Enter` → an accept of
the live row, status `reviewed <path>`; `n`/`Esc` → nothing written, the row stays pending).

**Why the question is about intent, not detection.** The Gate 8 sponsor run read the first
wording (`changed while your editor was open`) as "someone else touched this" after an
ordinary save. lastcall only knows the bytes differ from when the editor opened; it cannot
tell who wrote them — the editor's save and an agent's write look identical from outside —
but the *user* knows whether they saved, and the whole file was open, so the honest question
is whether every hunk in it is now reviewed. No count is shown: the row on screen may predate
the save. So a prompt after a session in which they saved nothing is
how an agent's write announces itself, and answering `n` costs nothing but a row that stays
pending. The guard for the other half — an agent writing the file **while** the session is
open — is not lastcall's at all: it is the editor's own changed-on-disk warning (vim's
`W12 Warning: File ... has changed since editing started`, VS Code's reload prompt, Emacs's
"has changed on disk; really edit the buffer?", Helix's `:w` refusal). lastcall relies on it
and says so here rather than pretending to a guard it cannot hold: for the duration of the
suspend lastcall is not running, has no terminal, and reads nothing.
The `left pending` case that is easiest to misread is the one verifier (a) F1 added: a
confirm, note or picker already on screen is **never** replaced by the return, because the
return arrives on a channel and the reader's next `y` would answer a question they never saw.
The row keeps the editor's delta either way, so nothing is lost — it is reviewed as an
ordinary pending row.

Effects queued while the editor owned the screen are drained in the pass after the resume,
and one of them may be the watcher's own notice of the editor's save, so the frame right
after a resume can already show the new pile.

### Select to copy (`v`, `y`), and OSC 52

The diff pane has no per-line cursor — `DiffCursor` is `{ hunk, scroll }` — so the selection
carries its own: `Sel { anchor, cursor }` over **absolute diff-line indices**. `v`
(`Action::Select`) anchors at the top visible line; while a selection is live `↑↓`, `PgUp`,
`PgDn` and the wheel move `sel.cursor` and `App::move_sel_cursor` scrolls the pane only
enough to keep the cursor on screen, so the selected lines stay under the reader's eye
instead of sliding off the top. `Esc` clears the selection and keeps the focus. Selected rows
are drawn full-width reverse-video (`render_hunks`).

`y` (`Action::Copy`) copies the selection's lines; **with no selection it copies the hunk
under the cursor whole**, header included, which is the common case and needs no `v` at all.
The payload is built by `app::diff_line_text` from the hunks, not from the screen: it keeps
tabs as tabs (matching `hunk_body` and the flag export) where the pane expands them, because
the paste target wants the file's own bytes. Both keys are guarded on
`effective_focus() == Focus::Diff`.

**The mouse does the same gesture.** A left press inside `HitMap.diff_body` takes the anchor
**before** `App::hit` runs — a press on a hunk header moves `diff.scroll`, so an anchor read
afterwards would be wrong — a drag sends `Action::SelectTo(line)`, and the release copies
*only* if the pointer actually moved. A press outside the diff body takes no anchor, which is
what keeps the divider drag a divider drag, and a press-and-release with no motion is still an
ordinary click (`run_mouse_drag_in_the_diff_selects_and_the_divider_drag_still_resizes`).

**Why OSC 52 and nothing else.** lastcall runs over ssh, inside tmux and inside a herdr pane,
where `pbcopy`/`xclip` would put the text on the *wrong* machine's clipboard. `ESC ] 52 ; c ;
<base64> BEL` asks the terminal the user is actually sitting at. The caveats are real and the
UI cannot hide them:

- **It is write-only.** The terminal never acknowledges, so lastcall cannot know the copy
  landed. The cue below says "we wrote it", not "you have it".
- **tmux drops it unless `set-clipboard on`** (the default is `external`, which forwards but
  does not set tmux's own buffer; `off` discards). Some terminals ship with OSC 52 disabled
  for security. If nothing arrives on the clipboard, that is where to look.
- **There is a size cap.** `clipboard::CAP` is 32 KiB of *payload*; terminals silently drop
  oversized sequences, and half a paste is worse than none. Over the cap lastcall writes
  nothing and says
  `selection too large to copy (N KiB; the terminal would drop it)` — and **keeps the
  selection**, because the only thing the reader can do about it is select less and they need
  the range in front of them to shrink it (`app_copy_over_the_cap_writes_nothing`).

The base64 is hand-rolled in `tui/clipboard.rs` (RFC 4648, no dependency) and pinned against
the RFC's own vectors; `Osc52` is a crossterm `Command`, so the escape goes out the same
`execute!` path every other terminal write uses.

**The cue.** A copy raises `App.cue` — a centered, reverse-video `copied to clipboard`
(`app::COPIED`) over the diff, for `CUE_SECS` = 2 seconds, cleared by the `Tick` arm. It is
deliberately **not** the status line: the status carries engine notices with a 30 s TTL, and
a copy must not evict `saved src/parse.rs`. `tui_copy_cue` pins a frame where both are on
screen at once.

## Keys

Defaults (`input::DEFAULT_KEYMAP`, in help-overlay order):

| action (the `[keys]` name) | default keys | in the nav | in the diff |
|---|---|---|---|
| `nav_up` / `nav_down` | `up` `k` / `down` `j` | previous / next entry | scroll one line |
| `nav_page_up` / `nav_page_down` | `pageup` `b` / `pagedown` `space` | a page of entries | a page of lines |
| `nav_top` / `nav_bottom` | `home` / `end` | the first / the last entry of `nav_entries()` | the diff's first line / the line a long `↓` run ends on (a live `v` selection's far end instead) |
| `nav_prev_root` / `nav_next_root` | `alt-up` `{` / `alt-down` `}` | the repository row of the listed root before / after the selection's own (`Selection::root()`); `{` inside the first root is that root's own row, `}` on the last is `Changed::No` | the same, and the focus comes back to the nav |
| `open` | `enter` `l` `right` | open the selected row's diff, cursor on that file's current hunk (on a root: its first row) | — |
| `back` | `esc` `h` `left` | — | back to the file list with the same row selected; closes help first; never quits |
| `focus_toggle` | `tab` | toggle focus between the panes | |
| `hunk_next` / `hunk_prev` | `n` `]` / `p` `[` | next / previous hunk (the current hunk's header is a full-width inverted band) | |
| `toggle_full_paths` | `f` | root-relative paths instead of basenames | |
| `toggle_remote` | `o` | show each repo's `org/repo` slug | |
| `hide_empty` | `t` | hide / show repos with nothing pending (§6.7 Amendment v1.9); default from `hide_empty_repos`, and independent of the `w` scope | |
| `snooze` | `s` | on a repo row: open the snooze modal (a day count), or wake a repo that is already snoozed ("The snooze modal" below); on any other row a notice, not a modal | |
| `show_snoozed` | `shift-s` | list the snoozed repos too, each with `snoozed until <date>` on its branch line | |
| `accept` | `a` | on a file row: the one hunk under the diff cursor (a hunkless row — binary, collapsed, deleted, unreadable — whole); on a group: the group; on a root: every row of it (asks above 10 files) | the same hunk |
| `accept_file` | `shift-a` | accept the selected file whole — the only key that does | |
| `accept_all` | `ctrl-a` | accept everything listed, every root (asks above 10 files) | |
| `restore` | `u` | put the hunk under the diff cursor back to its baseline (a hunkless, deleted, or one-hunk added row: the file, which asks) | the same hunk |
| `restore_file` | `shift-u` | put the selected file back whole — always asks first | |
| `undo` | `z` | reverse the selected repo's most recent accept: the paths it covered go back to pending and the selection moves to the first of them. Never touches the working tree; the stack is in the ledger, at most `ledger::UNDO_CAP` = 20 deep, and survives a restart | the same |
| `flag` | `m` | flag it with a note: the hunk under the diff cursor (an expansion's hunk counts), or the file from the nav | the same hunk |
| `unflag` | `shift-m` | clear every flag on the selected file | |
| `edit` | `i` | open the selected file in the **inline editor**, caret on the current hunk's first changed line ("The inline editor" below) | the same |
| `edit_external` | `shift-i` | suspend and open `$VISUAL`/`$EDITOR` on that file at that line ("`shift-i`" below) | the same |
| `select` | `v` | — (diff focus only) | start a line selection at the **top visible** diff line; `↑↓` then extend it |
| `copy` | `y` | — (diff focus only) | copy the selection, or the hunk under the cursor, over OSC 52 |
| `expand` | `e` | expand the selected collapsed row into hunks ("Collapsed rows" below) | |
| `ack` | `d` | ack the selected root's herdr ready flag ("herdr in the UI" below) | |
| `jump` | `g` | focus the selected root's agent in herdr | |
| `scope` | `w` | workspace scope on/off | |
| `refresh` | `r` | rescan every root now (ignored while one is running) | |
| `help` | `?` | the help overlay (any key closes it) | |
| `quit` | `q` `ctrl-c` | exit 0 | |
| `scroll_up` / `scroll_down` | *(unbound)* | bindable one-line diff scrolls | |

**The jumps are not Cmd bindings, because on macOS the Cmd key never reaches a terminal
program at all** — the terminal emulator keeps it — while Option-arrow arrives as `alt-up` /
`alt-down` in iTerm2 and in a herdr pane by default, and Terminal.app sends Option-arrow as a
word-jump escape unless its profile has "Use Option as Meta key" on, which is why `{` and `}`
are bound to the same two actions and work everywhere. Every one of the four moves goes
through `nav_entries()`, so a hidden root is not a stop on the way, and an empty nav answers
`Changed::No`. `pty_nav_jumps` drives all four through a real terminal (`End`, `Home`, `}`,
then `alt-up` as the raw `ESC [ 1 ; 3 A`).

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

**The help overlay is two columns when one does not fit.** With 37 bindable rows plus the
modal keys, a single column runs off the bottom of a 30-row terminal, so
`render::help_columns` splits the rows in half whenever one column would overflow the height
*and* the pair fits the width — each column sized to its own widest row, because padding both
to the widest row in the table costs the second column the width it needs. If two columns
would themselves have to be truncated, one column is no worse, and it stays. The vertical
clipping that follows eats key rows, never the footer: **four** rows are reserved out of
the truncation — the clip notice below, the newline note (`render::newline_note`), the
shift-drag note (`render::SELECT_NOTE`) and the `--tour` line (`render::TOUR_NOTE`) — with
`any key closes` out of the count by construction, since the last inner row is always its
own. The blank separator is not reserved; it is the first thing the clip spends. So the
whole footer survives at any size the overlay is drawn at. At 80 columns,
the standard width, the pair does not fit and the overlay clips: at 30 lines it reaches the
`quit` row, at 24 it stops earlier, and the footer is there either way (verifier (b) F4).
**What survives the clip is the table's order** (`input::DEFAULT_KEYMAP`), so that order is
a decision and not an accident: the review loop first (move, open, `t`, accept, restore,
`z` undo), then the keys that only change what the list shows (`e`, `f`, `o`), the snooze
pair, `?` and `q`. An 80×24 overlay has room for sixteen of those rows, and the fold
there falls after `z undo`; nothing of the loop is below it. The four jumps (2026-09-14)
come right after `z`, below that fold, because each is a shortcut for what a long `↓` run
already does. Their descriptions (`first entry / top of diff`, `last entry / end of diff`)
are kept within the 26 columns of `g jump to the agent in herdr`, the widest row of the
two-column form's right column: the split is by row count, so a longer description lands
on the right for some tables, the form then needs 103 columns instead of 100, and 100×30
folds sixteen keys. (Their first placement, right after the page keys, pushed `Ctrl-A`,
the two restores and `z` under the 80×24 fold, which reversed the verifier's F3 fix; the
order is a decision, so the fix was the order.)
A clipped overlay now **says** it is clipped (**ruling R12**): the row above the pinned
`quit` is a dim `… N more keys (100 columns shows all)`, so `q / Ctrl-C  quit` as the last
key row can no longer be read as the whole table.

Mouse: a left press on a nav entry selects it; on a hunk header it selects that hunk; on a
hunk header's `[a accept]` it accepts that hunk, on `[u restore]` it restores it and on
`[m flag]` it opens the note modal on it; on the main view's `[A accept file]` /
`[U restore file]` the file, on the header's `[Accept All]` everything listed; on the diff body it focuses the
diff; dragging the divider resizes the nav (clamped to 16..=60); the wheel scrolls the pane
under the pointer, three lines a notch.

**Selecting text.** `term::enter` turns mouse capture on, so a plain drag is ours, not the
terminal's. Inside the diff pane a plain drag is now lastcall's own line selection, which
copies on release ("Select to copy" below); anywhere else, hold **shift** while dragging to
select and copy with the terminal's own selection (every terminal we target honours the
shift override). The help overlay says both in its
second-to-last line (`render::SELECT_NOTE` — `shift+drag selects text (mouse capture is on) ·
v/y copies`); the last is `render::TOUR_NOTE`, `lastcall tui --tour shows the welcome again`.

### Undo (`z`) and snooze (`s`), and the snooze modal (Phase 10)

Both are `Pile` fields, which is what keeps `tui/app.rs` clean of the "never reads a ledger"
grep: the reducer reads `view.pile.undo` (a depth) and `view.pile.snoozed_until` (an ISO-8601
date, or `None`) and never opens anything.

**`z`** queues `Effect::Undo(root)` for the selected repo, and the engine reverses that
root's most recent accept. The reducer's own refusal path is `NOTHING_TO_UNDO`
(`nothing to undo`) for a pile that already says `undo: 0`; the engine refuses with the same
words for the race where the stack emptied since the frame. `UNDO_IN_PROGRESS` guards a
second `z` while one is running. When it lands, the selection moves to the first path the
undo put back, in path order, so the reader is looking at what came back rather than hunting
for it. `z undo` is on the hint line only while the depth is non-zero.

**`s`** on a repo row opens the snooze modal; on anything else it is the notice
`SNOOZE_NEEDS_ROOT` (`select a repository row to snooze it`) and no modal. On a repo that is
already snoozed — which you can only be looking at with `shift-s` on — `s` skips the modal
and wakes it (`Effect::Snooze { root, days: None }`).

The modal is the note modal's shape with a number where the text area is, because one number
is the whole question: `snooze <name> for [1▏] day(s)`, a blank, and
`digits edit   ⏎ snooze   Esc cancel` (`render::SNOOZE_KEYS`). It seeds with
`SNOOZE_DEFAULT_DAYS` = 1 and accepts up to `SNOOZE_MAX_DAYS` = 365; past that the digit is
**refused rather than clamped**, so the field never shows a number the write would not use.
An emptied field shows the caret alone rather than a `0` nobody typed, and `enter` on it
applies the default.

Snoozing is a **view**, exactly like the `w` scope and the `t` toggle: the repo is still
watched, still scanned, and `lastcall status` still reports it with a `snoozed_until`. The
`App::is_listed` clause reads
`snoozed_until.is_none() || show_snoozed || attention()` — the `hide_empty` exception
exactly, and for the same reason: an agent that is blocked or done is news the reader asked
for before they asked for quiet. Expiry is a **field test, never a clock read**
(design review F4): the engine stamps `None` for a deadline that has already passed under its
own injected clock, and `App::handle`'s `Tick` arm drops one that expires while the TUI is
open. There is no `SystemTime::now()` anywhere under `tui/`, `mod tests` included: the tests
that need an instant use a fixed one (`tour::tests::at`). The bottom line carries the
count as `N snoozed (S shows)`, and the key in the parenthetical comes from the keymap, so a
rebound `show_snoozed` renames the notice with it.

**Scenes.** `tui_undo_hint` (two frames, `tui_undo_hint_present` and
`tui_undo_hint_absent`), `tui_snooze_modal`, and `tui_scope_and_snooze_notice` at 100×30 and
80×24 for the combined bottom line; `pty_undo_file`, `pty_undo_accept_all` and
`pty_snooze_repo` in the PTY tier (that file's own `pty_*` convention, unlike the tour's
`tui_tour_*`).

### Collapsed rows and `e` (Phase 6)

A lockfile, a binary or a file over `collapse_size_bytes` is a **collapsed** row: `⊟` in the
nav, and in the main pane one dimmed line instead of a diff —
`collapsed (glob|binary|size) · +a −d` — with `[e expand]` right-aligned on it. The accept
story is unchanged and deliberately whole-row: `a` on a collapsed row takes the file (there
is no hunk to point at) and `A` does the same, which is why an expansion draws **no
per-hunk `[a accept]` control** — the row header's `[A accept file]` is the only accept on
that screen. `[u restore]` is off there for the same reason and one more: the row carries no
hunks, so a hunk restore would ask about nothing; whole-file restore is `U` (verifier (b)
F5). **`[m flag]` stays.** A flag only quotes — `m` on hunk 2 of 3 of an expansion writes a
flag about that hunk and the export says `hunk 2 of 3` — so `flag_target` reads
`App::view_hunks`, the hunks on screen, where accept and restore read the row.

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
`any key closes` are never columnised; they stay full width under the body, and the
truncation reserves all three of their rows (`cap - 3`, then `cap - 1`) rather than letting
the body's last row land on the footer's. Two columns need about 100 columns with these
descriptions, so 80 clips; shortening them to fit 80 would cost about twenty columns across
ten rows and was not worth the truth. The frames are
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

## Design pass inputs: the responsive rules (for the Phase 9 Claude Design pass)

The sponsor ruled at the Phase 7 close (§10 2026-09-05) that the bottom hint line and the
rest of the layout's fine-tuning belong to the Claude Design pass at the Phase 9 kickoff,
and that until then each phase makes its own judgment call and **records the dynamic
behaviour here** — "below N columns this happens, above N that happens" — so the pass can
review the screen holistically rather than one frame at a time. Every rule below is a
threshold in `render.rs`/`app.rs` with the snapshot that pins it; a phase that adds a
rule adds a row. Nothing here is a promise about the final design.

| Surface | Rule as built (Phase ≤ 7) | Pinned by |
|---|---|---|
| Whole frame | below `MIN_SIZE` = 40×10 the frame is only `too small: 40×10 min` and the hit map is empty | `tui_too_small_30x8`, `render_too_small_is_one_line` |
| Header (**Design pass D14**, confirmed as built) | `lastcall  N repos · N files · N hunks  [Accept All]` + the watch notice right-aligned; when the control and the notice do not both fit (about 60 columns) the control is dropped, **then the badge** (`^A` duplicates the control; nothing else says what is watched or whether herdr is answering); a notice that cannot fit beside the counts at all is dropped and the badge and control return, rather than leaving the right half empty. At 60 columns with the fixture's counts the ladder lands on counts + notice only — badge and control both gone (D14's earlier doc row said only the control was dropped) | `tui_narrow_60x20`, `render_narrow_header_keeps_the_notice_and_drops_the_control`, `render_header_drops_the_accept_control_before_the_herdr_badge` |
| Nav pane | outer width `App.nav_width`, 16..=60 (default 28), draggable; hidden below `NAV_MIN_COLS` = 70 columns, when the diff takes the whole body and has focus; keeps its scroll offset across selection changes | `tui_narrow_60x20`, `tui_nav_*` |
| Hint line (status bar) | built from the keymap, **ruling R4** (Phase 9a deliverable 4): every applicable hint is built, the whole line is tried, and while it does not fit **one hint at a time** is removed from a fixed drop order — `y copy`, `v select`, `r refresh`, `Tab focus`, `w scope`, `s snooze/wake`, `^A accept all`, `t hide/show empty`, `z undo`, `g jump`, `d ack`, `A accept file`, `n/p hunk`, the accept phrase (`HINT_DROP_ORDER`, keyed by **action name**, so a rebind moves the key and never the order). There are no width constants (D1's 110/128 were not built) and no all-or-nothing tiers. **The first two hints follow the focus** (Design pass D2, **ruling R3**, Phase 9a deliverable 3): with the nav focused the line opens `↑↓ select  ⏎ open`; with the diff focused `↑`/`↓` scroll a line and `⏎` does nothing (see Keys), so it opens `↑↓ scroll  ← back` — `back` is the keymap's own action and `←` its arrow spelling, so a rebind renames the hint (a keymap binding no arrow to `back` falls back to its first key). The two forms are exactly the same width, so no drop step moves; the 19 diff-focused frames say `↑↓ scroll  ← back`. **`? help  q quit` are never dropped and are always the last two hints on the line**, so a cut line still says where the rest of the keys are; the floor is `↑↓ select  ⏎ open  ? help  q quit` at 33 columns, inside `MIN_SIZE`'s 40. Below `NAV_MIN_COLS` = 70 the five that a nav-less frame cannot promise (`w scope`, `Tab focus`, `r refresh`, `v select`, `y copy`) are not offered whatever the arithmetic says. What is on the line follows the state: the accept phrase follows the selection (`a accept hunk  A accept file` / `a/A accept file` on a hunkless row / `a accept group` / `a accept all in <root>` on a **non-empty** repo row only — `a` on an empty one does nothing, verifier (a) F2); `t`'s label follows the toggle (`t hide empty` while showing all, `t show empty` while hiding); `v select  y copy` only with the diff focused; `d ack` only for a ready episode and `g jump` for either flag; `w scope` only under a scope. **A hint is not offered where its key answers nothing** (Phase 9b, verifier (b) F4): `n/p hunk` and `v select  y copy` are gated on `view_hunks()` being non-empty (so a repo row, an empty repo row, the all-clean state, and a collapsed, binary, deleted or unreadable entry drop them), and `^A accept all` on `counts_of(&AcceptScope::All).files > 0` — the same predicate the accept itself uses, so the hint and the key agree by construction rather than by a second rule that can drift. An inapplicable hint is never **built**, so it is not a hint the line is short of and `HINT_DROP_ORDER` is untouched: every width step keeps the order it had. `z undo` is offered only where the selected repo's `Pile::undo` is non-zero, on the same terms — so the key and the hint agree by construction, and the hint is what tells a reader the stack has anything in it. While a confirm modal is open the line is exactly `y confirm  n cancel  q quit`. `s snooze` is offered on a repository row only, and reads `s wake` on a snoozed one that `shift-s` is showing (the maintainer's own Phase 10 run: the wake was not apparent), so the label says which of the key's two jobs it will do; on a file row `s` refuses, so it is not offered there. The Phase 7 keys (`u`, `U`, `m`, `M`) and `S` are **not** on the hint line — they live on the hunk controls, the `?` overlay and the modal's own key row. | `render_hints_drop_one_at_a_time_from_the_right`, `render_hints_keep_help_and_quit_at_every_width` (40–70), `render_hints_at_80_keep_accept_file`, `render_hints_follow_the_selection` (the 124-column nav line, the 142-column diff line with its focus-true opening, the rebound-`back` case, and every step below them), `render_hint_line_names_the_toggle_by_state`, `render_hints_and_help_follow_the_app_keymap`, `render_hints_never_promise_a_key_that_answers_nothing`, `render_hint_line_offers_z_undo_only_when_there_is_something_to_undo`, `hints_offer_snooze_on_a_repo_row_and_wake_on_a_snoozed_one`, `tui_hint_diff_focus` (142×20), `tui_undo_hint`, `tui_narrow_60x20`, `tui_nav_empty_repo_row`, `tui_status_line_head_notice` |
| Status bar vs hints | the latest engine notice with its age replaces the hints for `STATUS_TTL` = 30 s, then the hints return | `tui_status_line_head_notice` |
| Scope notice | `scope: <ws> · N repos hidden (w shows all)` (43 columns) crowds the header at 100 columns — carried to the pass since Phase 5 | `tui_herdr_scope_notice`, `tui_herdr_scope_notice_with_status` |
| File header controls | `[A accept file] [U restore file]` right-aligned as one run; a run that does not fit is retried without its last label, so a narrow pane loses the newest control first and `[A accept file]` goes last | `tui_accept_controls`, `tui_narrow_60x20` |
| Hunk header controls (**Design pass D14**, confirmed as built) | `[a accept] [u restore] [m flag]` with the same drop-from-the-right rule; on an expansion hunk of a collapsed row only `[m flag]` is offered (restore of such a row stays whole-file); at 60 columns all three still fit with 12 spare columns and every label whole — the pass took the crowding the worker flagged and kept it: `[a] [u] [m]` would be roomier and would stop saying what the keys do | `tui_narrow_60x20`, `tui_diff_view_collapsed_expanded` |
| Flag marker | `  ⚑ <first line of the note>` on the file and hunk header in whatever columns remain after the path and the control run (`marker_budget`); nothing is drawn when fewer than the prefix fits | `tui_diff_view_flagged_hunk`, `tui_nav_flag_counts` |
| Help overlay (`?`; **ruling R12**, Phase 9a deliverable 7) | one column while the rows fit the height; two columns when they do not **and** the width allows (about 100 columns with these descriptions), gutter 3 — the pass confirmed two columns as the right answer for this keymap, and left *sections* as the v0.2 answer if it outgrows the pair; when neither fits (80×30 and below with this keymap) it clips key rows from the bottom, never the `newline_note`/`SELECT_NOTE`/`any key closes` footer (three rows reserved — the blank separator above them is not, and is the first row the clip spends). **A clipped overlay says so**: the last body row above the pinned `quit` becomes a dim `… N more keys (100 columns shows all)`, where N is exactly the **keys** not drawn — a two-column row carries two, which a row count got wrong (Phase 9a verifier (b) F1) — so `shown + hidden` is the whole table in either form and the column figure is `render::help_two_column_width` — the width the two-column form would need with this keymap, computed, not a literal. When the frame is already that wide the constraint is the height, so the remedy reads `a taller window shows all` instead. `quit` stays pinned under the notice, so a clipped overlay still ends with the two rows a reader needs (how to get out, and that there is more). **`any key closes` is drawn at every height** (Phase 9b, verifier (b) F5): it used to be written only where the body fell short of the box, so at the one height where the table fits exactly it was missing and a row shorter it came back with the clip notice. The last inner row belongs to that line unconditionally now — `cap` is the body's room, one short of the box's — and the clip path's arithmetic is unchanged (it already reserved the same row by counting four instead of three), so no clipped frame moved. The exact-fit height moves with the keymap, so the tests sweep for it rather than naming it | `tui_help_overlay` (100×30), `tui_help_overlay_tall` (100×45), `tui_help_overlay_80x24`, `tui_help_overlay_exact_fit`, `render_help_uses_two_columns_only_when_one_does_not_fit`, `render_help_says_any_key_closes_at_every_height`, `render_help_promises_shift_enter_only_with_enhancement` |
| Confirm modal | centered box, one question row from the scope; an accept shows live counts, a restore shows the one row; hint line switches to the modal's keys | `tui_restore_confirm`, `render_confirm_modal_shows_live_counts` |
| Note modal | centered, `NOTE_WIDTH` = 60 columns (clamped to the frame minus 4, floor 8), a fixed `NOTE_ROWS` = 5-line text area that scrolls to keep the caret visible, plus the title — which **names the target**, ` flag hunk 2 of 3 ` or ` flag whole file ` (agenda (d)) — the target line and the key row, `⏎ send   ^J newline   Esc cancel` or its `⇧⏎` form; bracketed paste is on only while it is open | `tui_note_modal`, `tui_note_modal_scrolled`, `tui_note_modal_whole_file`, PTY `pty_flag_note_exports_when_standalone` |
| Agent picker | centered, width = widest row + 4, height = rows + 2, both clamped to the frame; first row says the flag is already saved and `Esc` costs only the send; key row `↑↓ choose   ⏎ send   Esc cancel` | `tui_agent_picker` |
| Collapsed rows | `collapsed (binary) · +a −d · not expandable` / `collapsed (size)` with `[e expand]`; expansion capped at 2,000 lines with `… N lines omitted` | `tui_nav_collapsed_*`, `tui_diff_view_collapsed*` |
| Editor header (Phase 8; **ruling R10**, Phase 9a deliverable 6) | `editing <path> · line N/M[ · unsaved]`, bold, or red while a save stands refused. Only the path gives, and it gives from the **head**, cut at the leftmost `/` whose remainder fits (`render::ellipsize_head`); when not even `…<basename>` fits, the basename itself gives from the head (`…_for_a_file.rs` — the extension survives), because a basename floor overflowed into the row's tail clip and lost exactly the wrong parts (Phase 9a verifier (b) F2); ` · unsaved` is reserved at every width so the header does not shift on the first keystroke, and `line N/M` — the part that changes as you type — is never what is cut. At 60 columns with the fixture's `src/parse.rs` everything still fits, so `tui_editor_narrow_60x20` is unchanged | `render_ellipsize_head_cuts_at_a_slash_then_inside_the_basename`, `render_editor_header_keeps_the_position_and_reserves_unsaved`, `tui_editor_long_path_60x20`, `tui_editor_narrow_60x20`, `tui_editor_save_refused` (the red header) |
| Editor body (Phase 8) | five-column gutter, then the file with **no wrap**; a clipped row ends in a dim `→` and `End` is the way to the rest. The caret line is tinted (indexed 238) and the entered hunk banded (236), so the editor needs a 256-colour terminal to look right and degrades to "no tint" rather than to noise. Hint line becomes exactly `^S save   Esc close` | `tui_editor_open`, `tui_editor_narrow_60x20` (which asserts the `→` is on the frame) |
| Copy cue (Phase 8) | a centered one-line reverse-video `copied to clipboard` over the diff pane for 2 s; **not** the status line, so it cannot evict an engine notice, and the two can be on screen together | `tui_copy_cue`, `render_selection_is_reverse_video_and_the_cue_sits_over_the_diff` |
| Diff selection (Phase 8) | selected lines are full-width reverse video; the pane scrolls only enough to keep the selection's moving end visible, never a line per keystroke | `tui_diff_selection`, `app_select_extends_with_the_cursor_and_y_copies_the_range` |
| Nav listing (Phase 9a, §6.7 Amendment v1.9) | every repo under the parent is a nav row, pending or not: `is_listed` = not loading, no scope pending, in scope, **and** (`hide_empty` off, or the repo has rows, or a herdr attention flag). A repo with no rows is a dim name-and-branch row with no file rows under it, selectable like any other; its right pane reads `nothing pending in <name>` over the branch line. `nothing pending across N repos` is what the right pane says whenever **no listed repo has a pending row** — the ordinary all-clean launch included, where every repo is a nav row and none is selected; `select a file (↑↓ or click)` there invited choosing a file that is not on the frame (Phase 9a verifier (a) F5). Under an active scope that pane is the scope's own form (`nothing pending in <ws>` + the repos it covers + `N repos hidden (w shows all)`) whatever the hidden count, so a scope that hides only empty repos can no longer fall through to the global text and list the repos it is hiding (Phase 9a F6). The scope notice's `N` is `is_listed`'s rule minus the scope test, so it counts an empty out-of-scope repo while `t` is off — what `w` will actually reveal (Phase 9a F1). `t` (`hide_empty`) flips the filter; the header's `N repos` counts the repos on the nav, as it already did under a `w` scope. Accepting a repo's last file lands the cursor on that repo's own name row — the neighbour is the nearest surviving entry below **within the same repo**, else the nearest above (the name row is the last "above"), and only when the repo itself has left the nav the entry at its former nav index; never a wrap, never a jump into another repo while this one is listed | `tui_nav_empty_repo_row`, `tui_hide_empty_toggle`, `tui_accept_last_file_lands_on_the_repo_row`, `tui_empty_state`, `app_empty_root_is_listed_and_selectable`, `app_accept_last_file_selects_the_repo_row`, `app_reconcile_uses_the_same_neighbour_rule_as_advance`, `render_all_clean_frame_is_the_empty_state_not_a_prompt`, `app_scope_notice_counts_an_empty_out_of_scope_repo`, PTY `pty_accept_last_file_lands_on_the_repo_row_then_t_hides_it` |
| Launch hold (Gate 8 sponsor run; **rulings R5–R7**, Phase 9a deliverable 5) | nothing listed until every root reports; right pane `discovered N repos, checking status…` + one line per root; from 1 s a dim `K of N checked · F files pending so far · Ss` and a `✓` in the **leading column** of each reported root (D4 — the ticks line up, and the slow repo is the gap in the column); the header agrees with the pane (`lastcall  N repos · checking status…`, `[Accept All]` dim) and the status line carries the hints, not a second sentence about the same wait (D3); **repos** is the user-facing noun throughout, `root` stays the config and CLI word. The herdr scope wait no longer follows it as a second screen: the same frame continues with `… · F files pending · waiting for herdr scope…` on the counter line (D5, see "Herdr scope" below) | `render_loading_pane_counts_only_after_one_second`, `render_scope_pending_after_the_hold_keeps_the_root_list`, `app_loading_holds_the_listing_until_every_root_reports`, PTY `wait_first_piles` |

Open design questions the pass should take, in the order they have come up: whether the
hint line should carry `u`/`m` (or go to a second tier) once the width allows; the scope
notice at 100 columns; the select-to-copy cue's placement and
duration; and the two senses of "select" on the tier-3 hint line. (The editor header's own
question — path or position when the frame will not hold both — was answered by ruling R10:
the position always, the path head-ellipsized. The overlay question was answered by
**ruling R12** — two columns stay, and a clip says so; sections are the v0.2 answer, not a
third column. The 60-column crowding was answered by **D14**: both the header ladder and
the hunk band are confirmed as built, doc row corrected.)

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

Three rules from the Gate 8 sponsor run's launch flash (spec §10 2026-09-06 (iii)):

- **Our own pane never steers the scope.** herdr's `foreground_cwd` is the foreground
  process group's cwd, and in the pane lastcall runs in that group is lastcall and its `git`
  children — during the scans it pointed at whichever root was being scanned and the
  containment fallback followed it. `HERDR_PANE_ID` (also through `Env`) names our pane and
  `herdr::own_pane_scrubbed` clears its `foreground_cwd` before every derivation
  (`herdr_fold`); the shell `cwd` still places the pane.
- **Nothing is listed before the first verdict.** `HerdrView::scope_pending` is set at
  launch when a scope is configured and a workspace id is known, and `App::is_listed` is
  false while it holds. Any verdict clears it (`App::scope_settled`): a `Scope` update —
  even the `None` the view started with — a standalone start, a failed connect, a link that
  dropped before its snapshot. Without the hold the first pile was listed for one frame and
  hidden by the scope on the next.
- **The scope wait is the launch hold continuing, not a second screen** (Design pass D5,
  ruling R7). The `Loading` value is **kept** past the last report while `scope_pending`
  holds — `App::end_loading` flags it `scanned` instead of clearing it, and
  `scope_settled` is what drops it — so below `COUNTER_AFTER` the frame does not change at
  all, and from one second the same counter line carries the holding clause where
  `so far · Ss` was: `3 of 3 checked · 7 files pending · waiting for herdr scope…`, with
  every root ticked because every root has reported. The header keeps
  `N repos · checking status…` throughout. `render::SCOPE_PENDING` as a **standalone** dim
  line is what is left for a launch with no roots at all to hold.
- **The empty state under a scope names it.** With a scope active and nothing listed the
  pane reads `nothing pending in <label>`, the in-scope roots, and
  `N repos hidden (w shows all)`; `nothing pending across N repos` is only ever true with
  no scope hiding anything.

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

**Two rectangles sit beside the target list** rather than in it, because they answer "where
in this pane?" and not "what did I hit?": `HitMap.diff_body` (the hunk-lines rectangle,
narrower than `Target::DiffBody`) and `HitMap.editor` (the inline editor's text area, with
the gutter already subtracted). `run::diff_line_at` turns a press inside `diff_body` into an
absolute diff-line index for the selection anchor, and it is read **before** `App::hit` runs
— a press on a hunk header moves `diff.scroll`, so an anchor read afterwards would name a
different line. A press outside `diff_body` takes no anchor at all, which is exactly what
keeps a divider drag a divider drag.

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
- `pty_accept_refused_when_file_moves` — the `watching …` status is on (the watch is live,
  so the append below is the debounce's to find), `f1`'s diff open, the agent appends a
  line, `A` goes out before the 750 ms debounce has rescanned: the status reads `f1:
  changed since rendered; not accepted`, the row stays, the ledger has no override; once
  the rescan shows `M f1  +2 −1`, `A` accepts. Without that first wait a loaded runner
  let the FSEvents install land after `A`: its gap-closing rescan found the append and its
  `watching` notice replaced the refusal on the status row (CI macos-latest 2026-09-05).

Both print `PTY accept …` timing lines. The status bar is asserted as `<text> · <age>`
exactly, so `accepted f1` cannot pass for `accepted f1 · 1 hunk left`.

The Phase 8 scenes are the ones that need a **child process** and a real terminal handover,
which is exactly what neither a reducer test nor a snapshot can reach:
`pty_editor_save_pends_nothing` (`shift-i`, the probe editor rewrites the file, the return
confirm, `y`, and the row is gone), `pty_editor_ctrl_c_does_not_quit_lastcall` (a `^C` typed
while the editor owns the terminal kills the *editor*; lastcall is still up on resume, and
the scene then exits on a second `^C` 300 ms later — past `EDITOR_SETTLE`, so the drain
swallowed the editor's interrupt and not this one), `pty_editor_key_typed_during_the_editor_is_not_stuck` (an `n` typed while the probe editor
sleeps answers the return confirm on the resume, with no second keystroke — the DSR nudge of
step 5), `pty_edit_inline_save_pends_nothing` and
`pty_edit_inline_save_refused_when_the_file_moved` (`i`, type, `^S`, against a file an agent
rewrites underneath), `pty_copy_writes_osc52_with_the_selected_lines` (`vjjy`, then the raw
transcript is searched for exactly one `\x1b]52;c;` and its base64 decoded — by the test's own
RFC 4648 decoder, never by the encoder under test — back to the three rows that were on
screen) and `pty_keyboard_enhancement_probe_is_answered_and_swallowed`.
`isolated_lastcall` removes `$VISUAL` and `$EDITOR` from every child — no scene may reach the
developer's own editor — so an editor scene points `$EDITOR` at an absolute path inside its
own temp dir; `docs/dev/testing.md` has the probe script.

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
  all. Phase 8 adds two more environment reads under `tui/` and no others:
  `LASTCALL_KEYBOARD` (`term.rs`, ruling P9) and the `$VISUAL`/`$EDITOR` lookup that
  `run.rs` hands `EditorCommand::resolve` as a closure — the resolver itself is pure and
  reads nothing, which is why every row of its table is a unit test. Everything else still
  comes through the engine's `Env`; `textbuf.rs`'s `PROPTEST_CASES` read is inside
  `#[cfg(test)]`.

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
rg -n 'std::env::var|home_dir\(' crates/lastcall/src        # tui/term.rs: the two LASTCALL_LOG* reads and LASTCALL_KEYBOARD; tui/run.rs: the $VISUAL/$EDITOR closure it hands EditorCommand::resolve; commands/mod.rs: LASTCALL_PARALLELISM (test-only override, never under tui/); tui/textbuf.rs: PROPTEST_CASES, inside #[cfg(test)]; commands/update.rs: LASTCALL_UPDATE_BASE_URL, honoured only for loopback and only by the explicit command, plus HOME and CARGO_HOME for the package-manager refusals
rg -n '\.lock\(' crates/lastcall/src/tui                    # nothing (the bare `lock\(` this used to spell also matched `hunk_block(` and `modal_block(`)
rg -n 'Rendered::of' crates/lastcall/src                    # only tui/app.rs (requests come from the held rows)
rg -n 'last_pile|scan_all\(|\.scan\(' crates/lastcall/src/tui/app.rs   # nothing (the reducer never scans)
rg -n 'println!|eprintln!|print!' crates/lastcall/src/tui   # nothing (the messages are in commands/)
rg -n 'thread::sleep' crates/lastcall/src/tui               # nothing: no blocking sleep anywhere in the TUI
rg -n 'tokio::time::sleep' crates/lastcall/src/tui          # outside `mod tests`, exactly three, and none of them a fixed wait in the loop: `herdr.rs`'s `due_at` and `run.rs`'s rescan arm are `sleep_until(<deadline>)` inside a `select!` (the arm parks forever when there is no deadline), and `run.rs`'s `sleep(EDITOR_SETTLE)` is the `#[cfg(not(unix))]` half of `Signals::resume`, where unix drains the signal with `timeout_at` instead
rg -n 'lastcall_engine::herdr' crates/lastcall/src/tui     # only tui/herdr.rs and tui/run.rs (the task side); never app.rs or render.rs
rg -n 'e\.(restore|flag|unflag)\(' crates/lastcall/src     # only tui/run.rs (restore and flag reach the engine through one seam)
rg -n 'OpenOptions|File::create|fs::write' crates/lastcall/src   # tui/term.rs (the log file), tui/run.rs (the export fallback), tui/tour.rs (the first-launch marker under the state dir, plus two hits inside its own `mod tests`) and commands/update.rs (the O_EXCL temp beside the canonical `current_exe`, and the daily-check stamp under the state dir); no worktree file is ever opened for writing. The tour's *config* write is not here: it goes through the engine's `config::write`, the one place that may touch `config.toml`
rg -n 'e\.save\(|e\.read_rendered\(' crates/lastcall/src        # only tui/run.rs (the inline editor reaches the engine through one seam, like restore and flag)
rg -n 'Command::new' crates/lastcall/src/tui                # only tui/run.rs's `$EDITOR` spawn (Suspend::run); nothing else in the TUI starts a process (the update path's `curl` lives in commands/update.rs, outside tui/)
rg -n 'openat|renameat|OpenOptions|File::create|fs::write' crates/lastcall-engine/src --glob '!*test*'   # restore.rs is the only file that opens a path under a root, and config/write.rs the only one that opens `config.toml` (one `fs::write`, the temp beside the file it then renames over); every other hit writes under the state dir (the in-file `mod tests` of ops.rs, engine.rs, store.rs, index.rs, ledger.rs, roots.rs and config/write.rs account for the rest)
cargo tree -e normal -p lastcall -p lastcall-engine | grep -c testkit   # 0
```
