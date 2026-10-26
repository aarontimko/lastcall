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
  (defaults 300 and 3,000, about 36 MB of text per checkout, so one root's pending batch
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
with a deterministic, ever-changing edit of about a fifth of its lines, so each write
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

**Scale knobs:** every default above is a `const`. Three environment variables, read the
way `parallelism_env` reads its own (`test_bench.rs` around 48): `LASTCALL_BENCH_S5_SCALE`
(a float, default 1.0) multiplies the phase durations, and `LASTCALL_BENCH_S5_FILES` and
`LASTCALL_BENCH_S5_LINES` override the fixture's size, so a quick rehearsal and a
user-sized tree share one code path.

**Reading it:** absolute RSS varies 15 to 33 % between runs (bench.md's own notes), so
the curve is read as deltas within one run: quiet-end against quiet-end across the three
cycles, peak against quiet-end within a cycle. Like every other scenario, the first
numbers are recorded as two runs (A and B) with the machine block. A full run is about
17 minutes plus the fixture.

**Not in Part A:** any change under `crates/lastcall-engine/src` or
`crates/lastcall/src`; any change to S1 to S4; a gate.

## Part A gate

- [ ] `just bench-soak` runs S5 to completion on the release build at scale 1.0 and
      prints every metric above; at scale 0.2 it completes in under five minutes;
      the `time=` parser's test is a plain (not ignored) test in `test_bench.rs`, green
      under `just cargo test -p lastcall --test test_bench` (the debug build runs it and
      SKIPs the scenarios).
- [ ] A debug build prints S5's SKIP line and measures nothing, as the other scenarios.
- [ ] `just lint` and `just test-unit` green; no file outside `test_bench.rs`, the
      `justfile`, `docs/dev/bench.md`, `AGENTS.md` (one line for the recipe) and this
      kickoff changes.
- [ ] `docs/dev/bench.md` gains an S5 section: what it does, how to read the curve, and
      runs A and B with the machine block. The worker writes the section and runs one
      scale-0.2 rehearsal; the orchestrator runs A and B at scale 1.0 and fills the numbers.
- [ ] The orchestrator reads the curve and writes the Part B addendum: which of H1 to H6
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
