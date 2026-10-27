# Phase 17 kickoff: memory and CPU under many worktrees

Post-release mini-phase, opened 2026-10-25. Part A (this kickoff's build) only measures;
it changes no product code. Part B (the fixes) is ruled after Part A's numbers exist, in
an addendum to this file.

## Origin

The sponsor (2026-10-25): with several agents each working in a linked worktree of one
large repository, tens of thousands of changed lines pending, lastcall's memory reached
5 to 10 GB and its CPU about 10%, and the memory kept growing while they accepted. The
long-running instance on the maintainer's own machine (28 days, `~/dev/git`, 63 roots,
small piles) sits at 31 MB footprint, so the growth needs the heavy shape to appear.

The sponsor's constraint on everything that follows: no degradation of how rows and
hunks are tracked and shown; prefer changes with low or zero impact on that.

## Hypotheses (from reading the code at `657d3fd`; none measured; corrected by the design review)

- **H1, every diff resident.** `scan::Row.hunks` holds every changed line of every
  pending file as its own `Vec<u8>` (`hunks.rs` `Hunk.lines`), so a pile costs several
  times the changed text.
- **H2, whole blobs per scan.** A scan fetches the baseline and current blob of every
  pending text row into one `HashMap<Oid, Vec<u8>>` (`scan.rs` around 810 and 1010) and
  clones each pair again before diffing (`render_content`, around 936). Under the
  watcher the roots scan **one at a time** under the engine mutex (`watcher.rs` around
  1023), so the steady peak is one root's batch; only `scan_all` (launch and `r`) runs
  `MAX_PARALLELISM` = 8 roots at once, where the batches stack.
- **H3, no blob or diff reuse.** The stat cache exists and works (`update-index
  --refresh` on the private index, so unchanged files are not re-hashed), but every scan
  re-fetches every pending row's blobs and re-runs `hunks::counts` and `hunks::diff`,
  changed since the last scan or not.
- **H4, three to four whole-pile copies.** `RootState.last_pile` (`engine.rs` around
  1412), the app's view, `orphan_piles` for a root not yet synced, and, per accept,
  `AcceptRequest::All(view.pile.clone())` for every listed root (`app.rs` around 2571),
  alive for the accept's length. The watcher's channel applies backpressure and the
  screen drains up to 256 events a pass, so a queue of piles there is unlikely.
- **H5, parked trees uncapped.** `SeenCache` holds one full `ls-tree -r` listing per
  distinct parked or retired tree oid; parked records (live branches) have no cap. A new
  branch copies the record it left, so the cost grows only with distinct accepted trees.
- **H6, allocator high-water.** macOS malloc may keep freed regions resident, so the
  resident set can stay at its peak after accepts empty the list; that is retention, not
  a leak, and Part A must tell the two apart.

## Part A deliverable: scenario S5, the worktree soak

One new `#[ignore]`d scenario in `crates/lastcall/tests/test_bench.rs`, run by a new
recipe `just bench-soak` (release build, as `just bench`), and **not** added to
`just bench`, whose four-minute budget it would break. It is not a gate.

**Fixture** (built outside every measured region, under a `TempDir`):
- One repository of `S5_FILES` text files of `S5_LINES` lines each, committed
  (defaults 300 and 3,000, about 34 MB of text per checkout, so one root's pending batch
  is tens of MB and above the run-to-run noise bench.md records), plus `S5_WORKTREES`
  linked worktrees made with `git worktree add -b agent/wNN` (default 6) as siblings of
  the repository under the launch directory. The design review confirmed that shape
  lists seven roots at the default `search_depth` (a `.git` file counts, `roots.rs`),
  the six badged as worktrees.
- Isolation exactly as the other scenarios: `isolated_lastcall` (`HOME`, config and
  `LASTCALL_STATE_DIR` in the temp dir), no herdr in the environment, update check off,
  tour off. The TUI runs with the default timings (debounce 750 ms, 3 s cap), not
  `--poll 1`: the watcher's cadence is the user's.
- S5 spawns the binary **without** the harness's 10 ms RSS sampler (`Bench::tui` turns
  it on; S5 needs a variant that does not): its own 1 s sampler uses `PtyTui::pid()`.

**Writers:** one thread per worktree (the main checkout stays idle). Every
`S5_WRITE_MS` (default 250 ms) a writer rewrites the next file in its round-robin order
with a deterministic, ever-changing edit of one line in every `S5_STRIDE` (default 150:
line `j` is edited when `j % stride == seq % stride`, so 20 lines of a 3,000-line file,
150 apart, 20 separate hunks under the diff's 3 lines of context), so each write
both changes content already pending and, until every file has been touched, adds a
row. During churn phases only, every `S5_BRANCH_EVERY` writes (default 240, about a
minute) a writer commits what it has and switches to a new branch `agent/wNN-<k>` with
`git switch -c`, leaving the old branch in place, so parked records accumulate behind
accepts and name distinct trees (H5). Writer threads stop on a shared flag and are joined
before the quiet phase measures. Expected cadence: each written root scans about every
3 s (the debounce cap), six roots in turn; `cpu_pct` is spawn-bound scanning plus
diffing, a stress above the sponsor's 10%, read for its shape, not its level.

**Cycle** (run three times; the trend across cycles is what tells H6 from a leak):
1. **ramp** (default 90 s): writers on, nothing accepted.
2. **churn** (default 150 s): writers on, and every 20 s the harness sends `ctrl-a`
   (`accept_all`); it sends `y` only after the confirm modal's text appears (short
   timeout), never blind (a stray `y` is `copy`), and counts the accepts that actually
   started (an accept already running refuses a second one).
3. **quiet** (default 60 s): writers stopped and joined, one final `ctrl-a` (+ `y` when
   the modal appears), then idle; the screen should reach no pending rows.

After the third cycle, **relaunch**: writers on for 60 s with nothing accepted, quit the
TUI, start it again over the same state, and sample for 30 s. That is where `scan_all`
stacks up to eight roots' batches (H2's launch case).

**Sampling:** a harness thread samples the child once a second: resident set
(`ps -o rss=`) and cumulative CPU time (`ps -o time=`), CPU % taken from the time
deltas, not from `ps -o %cpu` (a decaying average on macOS). `time=` is parsed by
splitting on `:` and summing from the right with a fractional last field, because macOS
prints unbounded minutes (`3020:03.94`) and Linux `[DD-]HH:MM:SS`; a unit test covers
both. At each phase's end, when the tools exist (macOS; else the metric prints `n/a`):
- `footprint <pid>`: the `Footprint:` total (the parser reads the unit, KB, MB or GB)
  and the MALLOC rows' `Reclaimable` sum;
- `heap <pid>`: the malloc zones' in-use total. In-use low while footprint stays high is
  retention (H6); both high is live data (H1 to H4) or a leak;
- once at the very end, `leaks <pid>`: the leaked-allocation count and bytes,
  time-boxed (30 s), `n/a` on timeout.
No new dependency, no `libc`, no `getrusage`.

**Output**, as the other scenarios (`BENCH S5 <metric>=<value>` on stderr):
- one `BENCH S5 trace t=<s> cycle=<n> phase=<name> rss_kb=<n> cpu_pct=<n>` line every
  10 s (the curve), and one at every accept with `event=accept`;
- per cycle and phase: `c<n>_<phase>_end_rss_kb`, `c<n>_<phase>_peak_rss_kb`,
  `c<n>_<phase>_cpu_pct_avg`, `c<n>_<phase>_end_footprint_kb`,
  `c<n>_<phase>_end_reclaimable_kb`, `c<n>_<phase>_end_heap_inuse_kb`,
  `c<n>_churn_accepts_started`;
- `c<n>_quiet_settle_s`: seconds from the final accept to a screen with nothing pending
  (a backlog indicator, H4); `SKIP` when it never settles within the quiet phase;
- the header's file and hunk counts at the end of each ramp and churn phase, read off
  the screen;
- `relaunch_peak_rss_kb`, `relaunch_first_frame_s`;
- `leaks_count`, `leaks_bytes`;
- `parked_records_max` and `parked_trees_distinct` after the run, read from the temp
  state dir's ledgers with `serde_json` (a normal dependency of `crates/lastcall`), as
  counts only;
- the fixture's size (`fixture_mb`) and the run's wall time.

**Scale knobs:** every default above is a `const`. Four environment variables, read the
way `parallelism_env` reads its own (`test_bench.rs` around 48): `LASTCALL_BENCH_S5_SCALE`
(a float, default 1.0) multiplies the phase durations, `LASTCALL_BENCH_S5_FILES` and
`LASTCALL_BENCH_S5_LINES` override the fixture's size, and `LASTCALL_BENCH_S5_STRIDE`
(default 150) the writers' edit stride, so a quick rehearsal and a user-sized tree share
one code path.

**Reading it:** absolute RSS varies 15 to 33 % between runs (bench.md's own notes), so
the curve is read as deltas within one run: quiet-end against quiet-end across the three
cycles, peak against quiet-end within a cycle. Like every other scenario, the first
numbers are recorded as two runs (A and B) with the machine block. A full run is about
17 minutes plus the fixture.

**Not in Part A:** any change under `crates/lastcall-engine/src` or
`crates/lastcall/src`; any change to S1 to S4; a gate.

## Part A gate

- [x] `just bench-soak` runs S5 to completion on the release build at scale 1.0 and
      prints every metric above; at scale 0.2 it completes in under five minutes;
      the `time=` parser's test is a plain (not ignored) test in `test_bench.rs`, green
      under `just cargo test -p lastcall --test test_bench` (the debug build runs it and
      SKIPs the scenarios).
- [x] A debug build prints S5's SKIP line and measures nothing, as the other scenarios.
- [x] `just lint` and `just test-unit` green; no file outside `test_bench.rs`, the
      `justfile`, `docs/dev/bench.md`, `AGENTS.md` (one line for the recipe) and this
      kickoff changes.
- [x] `docs/dev/bench.md` gains an S5 section: what it does, how to read the curve, and
      runs A and B with the machine block. The worker writes the section and runs one
      scale-0.2 rehearsal; the orchestrator runs A and B at scale 1.0 and fills the numbers.
- [x] The orchestrator reads the curve and writes the Part B addendum: which of H1 to H6
      the numbers support, and the proposed fixes, each labelled by its impact on row and
      hunk tracking.

## Traps

- The writers must not starve the sampler or the PTY reader: keep their work small per
  tick and never hold a lock the harness needs.
- `ctrl-a` with nothing listed does nothing and opens no modal; the harness must not
  block waiting for one. Under `CONFIRM_ABOVE` (10 files) there is no modal either, and
  a blind `y` would run `copy`.
- A `git switch -c` in a worktree runs while lastcall may be scanning it; that is the
  real shape and not a harness bug. A writer's git failure (an `index.lock` taken by
  lastcall's read is impossible, lastcall never writes the user's index, but say so if
  one appears) is printed and the writer carries on.
- Tests never touch `~/.config/herdr/`, the real state dir, the real config, or
  `~/.local/bin/lastcall`.

## Design review, folded (2026-10-25)

Fresh-context reviewer, verdict "fixes required" (F1 to F6 gating); all thirteen folded,
none rejected. F1: watcher scans are serial, the 8-wide pool is launch and `r` only (H2
reworded, relaunch step added). F2: the stat cache exists; H3 is "no blob or diff
reuse". F3: no channel backlog (backpressure, 256 per drain); the copies are
`last_pile`, the view, `orphan_piles` and the accept request (H4 reworded). F4: parked
records share trees until an accept; branches switch in churn only, distinct trees
counted. F5: defaults raised to 300 x 3,000 lines with size knobs. F6: three cycles,
`heap` in-use, `Reclaimable`, `leaks`. F7: footprint's unit is parsed. F8: macOS
`time=` has unbounded minutes. F9: no 10 ms sampler in S5. F10: `y` only after the
modal appears; accepts started are counted. F11: the cadence is stated and the run is a
stress. F12: the hedges resolved (seven roots, `serde_json`, `std::env` in tests). F13:
wall time stated, two runs, read as within-run deltas.

## Code review, folded (2026-10-26)

Fresh-context reviewer over `de7ac76..a0fcc52`, verdict "fixes required" (F1 to F3
gating); all eight folded by the worker in `a4cdc16` and `a8ea01a`, none rejected. F1: the
`footprint` and `heap` readings are seconds apart under load, so the H6 pair is `heap`'s
own `Physical footprint` against its in-use total, one instant. F2: edits every fifth line
merged into one whole-file hunk, so the edit stride became a knob, default 150 (20
separate hunks per file). F3: the relaunch peak is read by the 10 ms sampler. F4 to F8:
reading notes in bench.md, the fixture size stated once, the tools' output out of the
state dir.

## Part B addendum: what the soak says (2026-10-26)

Runs A and B at stride 150 (20 hunks per file, 36,000 hunks over 1,800 pending files at
each ramp's end, the sponsor's "tens of thousands of lines") and run C at stride 5 (one
whole-file hunk per file, about a million changed lines), all at `bf975cb`, scale 1.0;
the numbers and the summary table are in `docs/dev/bench.md` (S5, runs A and B).

| | verdict | evidence |
|---|---|---|
| A leak | **no** | after each quiet phase's accept, malloc's live total is 2.4, 3.7 and 5.2 MB in all three runs, whatever the stride; `leaks` 0 |
| H1, every diff resident | **confirmed, the main cost** | same 1,800 files pending: live heap 52 to 133 MB at stride 150, 1,046 to 1,112 MB at stride 5; memory follows diff lines, not files |
| H2, blob batches, launch stacking | **confirmed** | at stride 150 the relaunch (827 and 835 MB RSS) is the run's highest point, above every churn peak (586 to 735 MB) |
| H3, no diff reuse | **supported** | lastcall's own CPU 27 to 32 % while six worktrees are written (git children excluded), 3 % idle; not isolated from scan spawns |
| H4, pile copies | **plausible** | at stride 5 the churn peaks (1,978 to 2,306 MB RSS) sit about 1 GB above the ramp end's live data; the accept's pile clone is the likeliest cause; not isolated |
| H5, parked trees | **negligible** | 42 distinct parked trees, MB-scale at most |
| H6, allocator retention | **confirmed** | with 2 to 5 MB live, the footprint Activity Monitor shows stays at 132 to 256 MB and RSS at 207 to 620 MB; macOS keeps what malloc freed |

The small quiet-end climb (about 1.4 MB a cycle, identical in every run) matches the undo
history: each accept-all keeps one entry per root naming every path it accepted, at most
`UNDO_CAP` = 20 per root, so it stops growing. To confirm in Part B, not a fix.

What this means for the sponsor's 5 to 10 GB: a pile's live cost is roughly five times its
diff text (changed lines plus three of context each side), held once by the engine and
once by the screen, copied again during an accept, and macOS keeps the high-water mark.
Run C reached 2.3 GB RSS with seven roots; a larger repository, more worktrees or larger
files reach the reported range by the same arithmetic.

### Proposed fixes, by impact on row and hunk tracking

The sponsor's constraint (2026-10-25): no degradation of how rows and hunks are tracked
and shown; low or zero impact preferred.

1. **Share a pile instead of copying it** (H4). The engine's last pile, the screen's view
   and an accept's request hold one shared, read-only pile (`Arc<Pile>`) instead of three
   clones. *Impact: none*; the same data is read by the same code.
2. **Store a hunk's lines in one buffer** (H1). `Hunk.lines` keeps every line as its own
   `Vec<u8>`; one byte buffer per hunk plus line offsets holds the same bytes in the same
   order with one allocation instead of one per line. *Impact: none*; a representation
   change behind the same accessors, covered by the snapshot and unit tests.
3. **Reuse a row's diff between scans** (H3). `hunks::diff` is a pure function of the two
   blobs, so a row whose baseline and current oids (and mode) are unchanged since the last
   scan takes the last scan's counts and hunks without re-reading either blob. The reuse
   holds only rows of the last pile, so it adds no memory. *Impact: none* on output;
   less CPU while agents write.
4. **Re-measure** with S5 (runs at stride 150 and 5) after 1 to 3, against this baseline.

Not proposed now, each with its reason:
- **Diffs on demand** (compute hunks only for the files on screen): the largest memory win,
  but it changes how hunks reach the view, the accept by hunk and the header's hunk count.
  *Impact: real*; held until 1 to 3 are measured.
- **A different allocator** (mimalloc or jemalloc) or asking malloc to return memory
  (`malloc_zone_pressure_relief`): a new dependency, or `unsafe` FFI, which the workspace
  forbids. 1 to 3 lower the high-water mark instead.
- **Fewer parallel scans at launch**: launch would take longer; re-measure first.

## Part B ruling (2026-10-26)

The sponsor, on the five recommendations put to them (fixes 1 to 3, re-measure before
deciding on diffs on demand, one pull request for the measurement and the fixes): "1 rec,
2 rec, 3 rec, 4 rec, 5 rec". On which number they had read: the 5 to 10 GB was the Memory
column of Activity Monitor, which is the footprint, so it is live or dirty memory and not
only freed pages the resident set still counts. Fixes 1 to 3 therefore target the number
the sponsor sees.

The one pull request carries Part A and Part B together from `perf/soak-bench`. It does
not release: there is no `just release-prep` commit unless the sponsor asks for one.

## Part B deliverable: fixes 1 to 3

Every change below is a representation or reuse change. **What a user sees and what is
stored must not change**:
- the TUI snapshots (`just test-e2e`) pass unchanged, with no `snapshots-update`;
- the `status --json` golden passes unchanged, with no `golden-update`;
- the state written to disk (ledgers, records, any serialized pile or hunk) is byte-for-byte
  what it was. Where a type is serialized, its serde shape is kept, by a hand-written
  `Serialize`/`Deserialize` if the field changes, and a unit test round-trips the old
  shape;
- the frozen §5 and §6 contracts are untouched.

**Fix 1, one shared pile.** The pile a scan produces is wrapped once in `Arc` and shared
by the engine's `RootState.last_pile`, the watcher's `EngineEvent` carrying it, the app's
`RootView.pile` and `orphan_piles`, and `AcceptRequest::All`. Every place that clones a
pile today clones the `Arc` instead. Where the app or engine mutates a pile it holds
(if any), it does so with `Arc::make_mut` or by building a new pile, never by a
`Mutex`. The worker lists every `pile.clone()` and `.pile.clone()` it found and what each
became.

**Fix 2, one buffer per hunk.** `Hunk.lines: Vec<(Tag, Vec<u8>)>` becomes one byte buffer
plus one `(Tag, end offset)` entry per line, behind accessors that give each line as
`(Tag, &[u8])` in the same order. Every reader (engine, `ops.rs`, `store.rs`, the TUI's
`render.rs`, `textbuf.rs`, `wrap.rs`, `app.rs`, the tests) moves to the accessors; no
reader sees the buffer directly. `hunks::diff` builds the buffer directly, without first
making a `Vec<u8>` per line.

**Fix 3, reuse a row's diff.** A scan that lists a row whose baseline oid, current oid and
modes are all equal to that path's row in the root's last pile takes the last pile's
counts and hunks for it (an `Arc` clone or a move, never a deep copy) and neither fetches
nor diffs the two blobs. Everything else about the row (status, flags, sort position, the
row cap) is computed as today. The cache is the last pile itself: no new map that
outlives it. A row whose key differs in any part, a binary or LFS row, and every row of a
root's first scan are computed as today. The key must include every input `hunks::diff`
and `hunks::counts` read (the reviewer checks this against the code: if a config value,
such as a skip rule or a size cap, changes their output, the reuse must also be dropped
when that value changes, or the value goes in the key).

**Not in Part B:** diffs on demand, a new allocator, any `unsafe`, any new dependency, a
change to S1 to S5's scenarios (S5 is re-run, not edited), a change to the scan's
parallelism.

## Part B gate

- [ ] Fixes 1 to 3 as above, each its own commit (or a few), conventional-commit titles.
- [ ] `just lint`, `just test-unit` and `just test-prepush` green; `just test-e2e` green
      with no snapshot rewritten; the `status --json` golden unchanged.
- [ ] New unit tests: the hunk buffer round-trips every line's tag and bytes (including a
      last line with no `\n`, an empty line and a CRLF line) and its serde shape equals the
      old one byte for byte; the diff reuse is taken when the key matches and not taken
      when the baseline oid, the current oid or the mode differs; a pile shared by the
      engine and the view is one allocation (`Arc::ptr_eq`).
- [ ] `just bench` S1 to S4 within bench.md's run-to-run noise of the last recorded runs,
      or better.
- [ ] S5 re-run by the orchestrator at stride 150 and stride 5, the numbers added to
      bench.md beside runs A to C, and the comparison written here: heap footprint (the
      Activity Monitor number), live heap at the ramp's end, churn peak, relaunch peak,
      CPU while writing.
- [ ] The sponsor rules on diffs on demand with those numbers in hand.
