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

## Hypotheses (from reading the code at `657d3fd`; none measured)

- **H1, every diff resident.** `scan::Row.hunks` holds every changed line of every
  pending file as its own `Vec<u8>` (`hunks.rs` `Hunk.lines`), so a pile costs several
  times the changed text.
- **H2, whole blobs per scan.** `scan.rs` (the per-row diff, around line 936) reads the
  full baseline and current blob of every pending text row and clones each before
  diffing; up to `MAX_PARALLELISM` = 8 roots scan at once, so these peaks stack.
- **H3, nothing reused between scans.** Every scan re-diffs every pending row, changed
  since the last scan or not: the CPU cost under constant writes.
- **H4, copies in flight.** A pile lives in `RootState.last_pile`, in the watcher's
  `EngineEvent::Pile` channel (bounded at 256 events, `watcher.rs`), and in the app's
  view; a screen that drains slower than the scans produce lets piles queue.
- **H5, parked trees uncapped.** `SeenCache` holds a full `ls-tree -r` listing per
  distinct parked or retired tree; parked records (live branches) have no cap.
- **H6, allocator high-water.** macOS malloc may keep freed regions resident, so the
  resident set can stay at its peak after accepts empty the list; that is retention, not
  a leak, and Part A must tell the two apart.

## Part A deliverable: scenario S5, the worktree soak

One new `#[ignore]`d scenario in `crates/lastcall/tests/test_bench.rs`, run by a new
recipe `just bench-soak` (release build, as `just bench`), and **not** added to
`just bench`, whose four-minute budget it would break. It is not a gate.

**Fixture** (built outside every measured region, under a `TempDir`):
- One repository of `S5_FILES` text files of `S5_LINES` lines each, committed
  (defaults 400 and 250: 100,000 lines), plus `S5_WORKTREES` linked worktrees made with
  `git worktree add -b agent/wNN` (default 6). The worktrees sit where discovery lists
  each as a root of its own at the default `search_depth` (siblings of the repository
  under the launch directory are the simplest; the worker checks `roots.rs` and says
  which shape it used). Seven roots in all.
- Isolation exactly as the other scenarios: `isolated_lastcall` (`HOME`, config and
  `LASTCALL_STATE_DIR` in the temp dir), no herdr in the environment, update check off,
  tour off.

**Writers:** one thread per worktree (the main checkout stays idle). Every
`S5_WRITE_MS` (default 250 ms) a writer rewrites the next file in its round-robin order
with a deterministic, ever-changing edit of about a fifth of its lines, so each write
both changes content already pending and, until every file has been touched, adds a
row. Every `S5_BRANCH_EVERY` writes (default 240, about a minute) a writer commits what
it has and switches to a new branch `agent/wNN-<k>` with `git switch -c`, leaving the
old branch in place, so parked records accumulate as an agent's would (H5). Writer
threads stop on a shared flag and are joined before the quiet phase measures.

**Cycle** (run twice; the second cycle is what tells H6 from a leak):
1. **ramp** (default 120 s): writers on, nothing accepted.
2. **churn** (default 180 s): writers on, and every 20 s the harness sends `ctrl-a`
   (`accept_all`) and confirms the modal (`y`) when it appears.
3. **quiet** (default 90 s): writers stopped and joined, one final `ctrl-a` + `y`, then
   idle; the screen should reach no pending rows.

**Sampling:** a harness thread samples the child once a second: resident set
(`ps -o rss=`) and cumulative CPU time (`ps -o time=`), CPU % taken from the time
deltas, not from `ps -o %cpu` (a decaying average on macOS). At each phase's end it also
reads `footprint <pid>`'s `Footprint:` line when that tool exists (macOS), which is the
number Activity Monitor shows; absent, the metric is printed as `n/a`. No new
dependency, no `libc`, no `getrusage`.

**Output**, as the other scenarios (`BENCH S5 <metric>=<value>` on stderr):
- one `BENCH S5 trace t=<s> cycle=<n> phase=<name> rss_kb=<n> cpu_pct=<n>` line every
  10 s (the curve);
- per cycle and phase: `c<n>_<phase>_end_rss_kb`, `c<n>_<phase>_peak_rss_kb`,
  `c<n>_<phase>_cpu_pct_avg`, `c<n>_<phase>_end_footprint_mb`;
- `c<n>_quiet_settle_s`: seconds from the final accept to a screen with nothing pending
  (a backlog indicator, H4); `SKIP` when it never settles within the quiet phase;
- the header's file and hunk counts at the end of each churn phase, read off the
  screen, so the run shows accepts keeping up or not;
- `parked_records_max` after the run, read from the temp state dir's ledgers with
  `serde_json` (already a dev-dependency? the worker checks; if not, a plain substring
  count is enough).

**Scale knobs:** every default above is a `const`; one environment variable,
`LASTCALL_BENCH_S5_SCALE` (a float, default 1.0), multiplies the three phase durations
so a quick rehearsal (`0.2`) and the full soak share one code path. Read it the way
`parallelism_env` reads its variable in that file.

**Not in Part A:** any change under `crates/lastcall-engine/src` or
`crates/lastcall/src`; any change to S1 to S4; a gate.

## Part A gate

- [ ] `just bench-soak` runs S5 to completion on the release build at scale 1.0 and
      prints every metric above; at scale 0.2 it completes in under three minutes.
- [ ] A debug build prints S5's SKIP line and measures nothing, as the other scenarios.
- [ ] `just lint` and `just test-unit` green; no file outside `test_bench.rs`, the
      `justfile`, `docs/dev/bench.md`, `AGENTS.md` (one line for the recipe) and this
      kickoff changes.
- [ ] `docs/dev/bench.md` gains an S5 section: what it does, how to read the curve, and
      the first run's numbers with the machine block.
- [ ] The orchestrator reads the curve and writes the Part B addendum: which of H1 to H6
      the numbers support, and the proposed fixes, each labelled by its impact on row and
      hunk tracking.

## Traps

- The writers must not starve the sampler or the PTY reader: keep their work small per
  tick and never hold a lock the harness needs.
- `ctrl-a` with nothing listed does nothing and opens no modal; the harness must not
  block waiting for one. Under `CONFIRM_ABOVE` (10 files) there is no modal either.
- A `git switch -c` in a worktree runs while lastcall may be scanning it; that is the
  real shape and not a harness bug. A writer's git failure (an `index.lock` taken by
  lastcall's read is impossible, lastcall never writes the user's index, but say so if
  one appears) is printed and the writer carries on.
- Tests never touch `~/.config/herdr/`, the real state dir, the real config, or
  `~/.local/bin/lastcall`.
