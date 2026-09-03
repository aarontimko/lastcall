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
  count reads `N+` when any root's pile stopped at the engine's row cap), the body — a nav pane
  (outer width `App.nav_width`, 16..=60, default 28; hidden below `NAV_MIN_COLS` = 70
  columns, when the diff takes the whole body and has focus) sharing its right border with a
  bordered main pane — and a one-line status bar (the latest engine notice with its age for
  `app::STATUS_TTL` = 30 s, then the key hints). Below `MIN_SIZE` (40×10) the whole frame
  is `render::TOO_SMALL` (`too small: 40×10 min`) and the hit map is empty. Only the visible
  window of nav entries and diff lines is built, so a 50 000-line diff costs the same as a
  50-line one. The help overlay (`?`) and the accept confirm modal are drawn last over
  everything.
- **`input.rs` — `Action` and the keymap.** Every key, mouse gesture and the 1 s tick becomes
  one `Action` before it touches `App` (`to_action(&Event, &Keymap)`), so the reducer never
  sees a crossterm type and the keyboard and mouse paths are provably equivalent — the seven
  parity tests `input_parity_select_repo`, `input_parity_select_file`,
  `input_parity_hunk_next`, `input_parity_hunk_prev`, `input_parity_accept_hunk`,
  `input_parity_accept_file`, `input_parity_accept_all` drive the same scene by key and by a
  click resolved through the hit map and assert the same `App` (and, for the accepts, the
  same `Effect`). The confirm modal's keys (`input::MODAL_KEYS`: `y`/`enter` confirm,
  `n`/`esc` cancel) are not in the keymap: `Ui::event` resolves them through `modal_action`
  before the keymap while the modal is open, and swallows every other key.
- **`run.rs` — the loop.** One tokio `select!` over the watcher's events, the terminal
  reader thread's events, the loop's own finished engine work (`Local`), a 1 s tick, Ctrl-C
  (a key event under raw mode; the signal branch is for `kill -INT`) and SIGTERM. Every
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

### Startup and the first frame

`commands/tui.rs` fails loudly *before* the terminal is touched: stdout not a TTY → the
one permitted message `lastcall: not a terminal; try \`lastcall status\`` on stderr, exit 2
(never draws into a pipe); a `[keys]` table that does not parse → `lastcall: [keys] …`, exit
2; an engine that cannot open → exit 1 like `status`. Then `run::run` builds the runtime as
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

- **Scope** (`App::accept_scope`, `AcceptScope`): `a` on a file row with the diff focused
  is the hunk under the cursor; on a file row otherwise, the whole file; on a group entry,
  the group; on a root entry, every row of that root (the per-repo fold). `A` is the whole
  file from either pane; `ctrl-a` and the header's `[Accept All]` are every listed root.
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
  advance rule; then one status line — `accepted f1 · hunk 2 of 3` (the index and count
  are the row's at the moment the accept was asked; each rescan shrinks the count),
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
  Focus stays where it was. A refusal leaves the selection where it was.
- **The confirm modal.** An accept covering more than `CONFIRM_ABOVE` = 10 files asks
  first (10 accepts, 11 asks). `App.confirm` stores only the scope; the numbers shown are
  `confirm_counts()` from the held piles at *every* render, so a pile applied under the
  open modal changes them and `Confirm` folds exactly what is shown (if the scope empties
  underneath, the modal closes with `nothing to accept`). While the modal is open every
  action but `Tick`, `Resize`, `Confirm`, `Cancel` is ignored, and every key but its own is
  swallowed before the keymap — so `Esc` cancels without also going back.
- **The seq rule (the §11 hardening).** `App.seq` remembers the last scan seq applied per
  root; a `Pile` event with a lower seq — a watcher scan that was already running when the
  accept took the lock — is `Changed::No` and touches nothing, through either channel
  (`app_older_seq_pile_is_dropped_untouched`,
  `run_stale_watcher_pile_after_accept_is_dropped`). The entry is removed when the root is
  removed, so a re-added root receives piles again.
- **Hints follow the selection** so the per-repo fold and the global one are told apart:
  `a accept hunk  A accept file` on a file row with the diff focused, `a/A accept file`
  otherwise, `a accept group`, `a accept all in <root>`, and `^A accept all` always. When
  the line would not fit it drops `Tab focus  r refresh` first (always below 70 columns),
  then the file and global accept hints.

## Keys

Defaults (`input::DEFAULT_KEYMAP`, in help-overlay order):

| action (the `[keys]` name) | default keys | in the nav | in the diff |
|---|---|---|---|
| `nav_up` / `nav_down` | `up` `k` / `down` `j` | previous / next entry | scroll one line |
| `nav_page_up` / `nav_page_down` | `pageup` `b` / `pagedown` `space` | a page of entries | a page of lines |
| `open` | `enter` `l` | open the selected row's diff (on a root: its first row) | — |
| `back` | `esc` `h` | — | back to the file list; closes help first; never quits |
| `focus_toggle` | `tab` | toggle focus between the panes | |
| `hunk_next` / `hunk_prev` | `n` `]` / `p` `[` | next / previous hunk (the current hunk's header is drawn inverted) | |
| `toggle_full_paths` | `f` | root-relative paths instead of basenames | |
| `toggle_remote` | `o` | show each repo's `org/repo` slug | |
| `accept` | `a` | accept the selected entry: a file, a group, or every row of a root (asks above 10 files) | accept the hunk under the cursor |
| `accept_file` | `shift-a` | accept the selected file whole | |
| `accept_all` | `ctrl-a` | accept everything listed, every root (asks above 10 files) | |
| `refresh` | `r` | rescan every root now (ignored while one is running) | |
| `help` | `?` | the help overlay (any key closes it) | |
| `quit` | `q` `ctrl-c` | exit 0 | |
| `scroll_up` / `scroll_down` | *(unbound)* | bindable one-line diff scrolls | |

The confirm modal answers only `y` / `enter` (confirm) and `n` / `esc` (cancel); these are
fixed (`input::MODAL_KEYS`), not `[keys]` names, and the help overlay lists them last.

Mouse: a left press on a nav entry selects it; on a hunk header it selects that hunk; on a
hunk header's `[a accept]` it accepts that hunk, on the main view's `[A accept file]` the
file, on the header's `[Accept All]` everything listed; on the diff body it focuses the
diff; dragging the divider resizes the nav (clamped to 16..=60); the wheel scrolls the pane
under the pointer, three lines a notch.

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
`HunkAccept(i)`) and calls `App::hit`, which is the same reducer path the equivalent key
takes (`app_hunk_click_equals_hunk_key`, `app_accept_hunk_by_keys_equals_hunk_accept_click`).
While the confirm modal is open `hit` ignores every target.

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

## Probes and logging

- `just probe-tui` — release build, a fixture parent in `/tmp/lc-probe-<pid>/`, then the
  interactive TUI over it with `--poll 1`; prints the `LASTCALL_CONFIG` / `LASTCALL_STATE_DIR`
  lines first so you can re-run by hand and edit a fixture file from another terminal.
- `just probe-tui-screen` — the transcript form: the PTY harness drives the release binary
  over the same fixture, appends a line to `alpha/f1`, waits for the row's counts to change,
  opens the diff and prints the screen as text plus the exit code after `q`. About three
  seconds; nothing is left behind.
- `LASTCALL_LOG_FILE=/path/to/log lastcall` — `tracing` never writes to the terminal while
  the screen is up; with this variable set the TUI appends to that file, filtered by
  `LASTCALL_LOG` (an `EnvFilter` directive, default `info`). Unset, there is no subscriber at
  all. These two reads are the only environment access under `tui/`; everything else comes
  through the engine's `Env`.

## Gate greps

```sh
rg -n 'Command::new\("git"\)' crates                       # engine git.rs, plus the testkit's fixture builder; nothing under tui/
rg -n 'std::env::var|home_dir\(' crates/lastcall/src        # only the LASTCALL_LOG* reads in tui/term.rs
rg -n 'lock\(' crates/lastcall/src/tui                      # nothing
rg -n 'Rendered::of' crates/lastcall/src                    # only tui/app.rs (requests come from the held rows)
rg -n 'last_pile|scan_all\(|\.scan\(' crates/lastcall/src/tui/app.rs   # nothing (the reducer never scans)
rg -n 'println!|eprintln!|print!' crates/lastcall/src/tui   # nothing (the messages are in commands/)
rg -n 'thread::sleep|tokio::time::sleep' crates/lastcall/src/tui   # nothing
cargo tree -e normal -p lastcall -p lastcall-engine | grep -c testkit   # 0
```
