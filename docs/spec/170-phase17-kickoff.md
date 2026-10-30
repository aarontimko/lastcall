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

## Part B deliverable: fixes 1 to 4

Every change below is a representation or reuse change. **What a user sees and what is
stored must not change**:
- the TUI snapshots (`just test-e2e`) pass unchanged, with no `snapshots-update`;
- the `status --json` golden passes unchanged, with no `golden-update`;
- the ledgers and records on disk are byte-for-byte what they were. No ledger, record or
  `status --json` serializes a `Hunk` or a `Pile` (`status.rs` emits `row.hunks.len()`);
  hunk bytes reach disk only through a hunk flag, `FlagHunk { header, text }`, built by
  `hunk_header` and `hunk_body` (`app.rs` around 5433 to 5462) and read by
  `flags::export`, so those outputs are byte-for-byte invariants of fix 2. The serde
  derives on `Pile`, `Row` and `Hunk` serve the test fixture
  `crates/lastcall/tests/fixtures/piles_three_roots.json`: it must still load and
  re-serialize identically;
- the frozen §5 and §6 contracts are untouched.

**Fix 1, one shared row list.** `Pile.rows: Vec<Row>` becomes `Arc<Vec<Row>>`; the small
per-scan fields (`notices`, `omitted`, `snoozed_until` and the rest) stay plain, so
`Pile::clone` is cheap and every existing `pile.clone()` (the engine's `last_pile`
(`engine.rs` around 1412 and 1746), `Accepted.pile` (around 1755), the watcher's
`EngineEvent::Pile`, the app's `RootView.pile` and `orphan_piles`, `AcceptRequest::All`
(`app.rs` around 2571 and 2582), accept-by-hunk's row clone (around 2514)) shares the
rows instead of copying them. The snooze expiry's field write (`app.rs` around 3907) stays
a field write and copies nothing. Code that builds or sorts rows does so on a `Vec<Row>`
and wraps it once; nothing calls `Arc::make_mut` on a shared pile. The workspace's `serde`
gains the `rc` feature (a feature, not a new crate; `Cargo.lock` unchanged) so `Arc`
fields keep their JSON shape. `std`'s `Arc` equality short-cuts on pointer identity for
`Eq` types, so `apply_pile`'s whole-pile compare (`app.rs` around 2266) gets cheap for
shared rows; keep that compare as it is. The worker lists every pile and row clone it
found and what each became.

**Fix 2, one buffer per hunk.** `Hunk.lines: Vec<(Tag, Vec<u8>)>` becomes one byte buffer
plus one `(Tag, end offset)` entry per line (`u32` offsets are enough; a hunk over 4 GiB
is refused by the size collapse long before), behind accessors that give each line as
`(Tag, &[u8])` in the same order, and a `truncate_lines(n)` for `hunks::truncate`
(`hunks.rs` around 238, the one mutator). The buffer is built presized (the sum of the
line lengths is known) so `Vec` doubling gives none of the saving back. Readers outside
`hunks.rs`: `scan.rs` (around 985), `engine.rs` (around 1601), `ops.rs`, `app.rs` (around
5437 to 5545), `render.rs` (around 1964 and 2128), `wrap.rs` (around 281), `tour.rs` and
the tests. `textbuf.rs` does not read hunks and is not touched. Expected effect: today a
38-byte line costs about 80 bytes; after, about 46 to 56, a 30 to 40 % cut of hunk memory.

**Fix 3, reuse a row's diff.** What `render_content` (`scan.rs` around 912 to 995)
produces from a row's blobs is `(added, deleted, hunks, collapsed)`; that set is what is
reused, never part of it. A row takes the reuse when all of these hold, else it is
rendered as today:
- the same path has a row in the root's last pile, with equal `baseline` and `current`
  entries (oid and mode each);
- that earlier row was rendered with its blobs in hand: not from a scan whose batch
  failed (`scan.rs` around 816 to 823) and with neither side missing from the store
  (around 943 to 951). The engine keeps this as a set of reusable paths on `RootState`
  beside `last_pile`, never a serialized field;
- the `collapsed_globs` and `collapse_size_bytes` the last pile was built with equal
  today's (a reload, `engine.rs` around 784, can change both); store them, or a
  generation counter `reload` bumps, beside `last_pile`. `store.filemode()` is fixed for
  a `Store` and needs no key;
- the row is not `Collapsed::Unread`, `Typechange` or `Unreadable` (those never read
  blobs today and keep their own path).
Reused rows are left out of `wanted` (`scan.rs` around 808), so their blobs are neither
fetched nor diffed. Their hunks are shared, not copied: `Row.hunks` becomes
`Arc<Vec<Hunk>>` (with fix 1's `rc` feature keeping the JSON shape), built as a `Vec`
in `render_content` (the mode-change hunk's push included) and wrapped once. The cache is
the last pile itself: no new map that outlives it. In the same commit, two cheap wins:
`render_content` borrows the blobs instead of `b.clone()` (around 945); and, only if a
property test proves `hunks::counts(old, new)` equals the sum of the diff's hunk counts
for every input, a non-collapsed row takes its counts from the diff instead of running
the diff twice (otherwise both calls stay).

**Fix 4, fetch blobs in chunks.** `cat_blobs` returns one map of every pending row's two
whole blobs for a root (`scan.rs` around 808 to 816), and at launch eight roots do so at
once: that is the run's highest point (relaunch, 827 to 835 MB RSS at stride 150).
`render` fetches and renders the rows in chunks of at most `BLOB_CHUNK_BYTES` (a `const`,
64 MiB, sized from the rows' blob sizes, which the scan knows, else a row count) with one
`cat-file --batch` call per chunk: the same rows, the same order, the same notices, and
the chunk's map is dropped before the next is fetched. A whole-batch failure still fails
open the whole scan's content, as today (`scan.rs` around 816 to 823: zero counts, no
hunks, one notice), not chunk by chunk, so what a broken store shows is unchanged.
No change to the scan's parallelism.

**Not in Part B:** diffs on demand, a new allocator, any `unsafe`, any new crate, a change
to S1 to S5's scenarios (S5 is re-run, not edited), a change to the scan's parallelism.

## Part B gate

- [x] Fixes 1 to 4 as above, each its own commit (or a few), conventional-commit titles,
      in the order 1, 2, 3, 4.
- [x] `just lint`, `just test-unit`, `just test-prepush` and `just test-scenarios` green;
      `just test-e2e` green with no snapshot rewritten; the `status --json` golden
      unchanged; the pile fixture loads and re-serializes identically.
- [x] New tests:
  - the hunk buffer round-trips every line's tag and bytes (a last line with no `\n`, an
    empty line, a CRLF line), and `truncate_lines` matches the old `truncate`;
  - `hunk_header`, `hunk_body` and `flags::export` give the same bytes as before fix 2
    over the fixture's hunks;
  - fix 3: a property test scans a randomly edited repository twice, with the reuse on
    and forced off, and the piles are equal; unit cases where the reuse is not taken
    (baseline oid, current oid or mode differs; the last scan's batch failed; a side was
    missing; `collapsed_globs` or `collapse_size_bytes` changed by a reload between the
    scans) and one integration case for the reload; `just test-scenarios` also passes
    with the reuse forced off (a test-only switch, not a config key);
  - fix 1: a pile held by the engine and by the view shares its rows (`Arc::ptr_eq`);
  - fix 4: a root whose blobs span several chunks gives the same pile as one chunk, and a
    whole-batch failure still fails open as before.
- [x] `just bench` S1 to S4 within bench.md's run-to-run noise of the last recorded runs,
      or better.
- [x] S5 re-run by the orchestrator: after each fix's commit, once at stride 5 and
      `LASTCALL_BENCH_S5_SCALE=0.34` (attribution: ramp-end live heap, relaunch peak);
      after all four, full runs at stride 150 and stride 5, added to bench.md beside runs
      A to C. Expectation: ramp-end live heap at stride 5 down from about 1,050 MB to
      roughly 300 to 400 MB, and the relaunch peak lower; a worse number is a finding.
      The comparison is written here: heap footprint (the Activity Monitor number), live
      heap at the ramp's end, churn peak, relaunch peak, CPU while writing.
- [x] The sponsor rules on diffs on demand with those numbers in hand: held (below).

## Design review of Part B, folded (2026-10-26)

Fresh-context reviewer over `7c4e8bd`, verdict "fixes required" (F1 to F4 gating); all
twelve folded, none rejected. F1: the reuse covers `collapsed` too, and drops on a reload
that changes the collapse rules. F2: rows from a failed batch or with a missing side are
never reused. F3: `Row.hunks` becomes `Arc`, serde's `rc` feature keeps the JSON shape.
F4: the snooze write would deep-copy an `Arc<Pile>`, so the `Arc` moves to the rows. F5:
the relaunch peak was untouched by fixes 1 to 3, so fix 4 (chunked blob fetch, no output
change) was added under the delegated-rulings practice and is listed for the sponsor at
close. F6: borrow the blobs; one diff run when a property test proves the counts equal.
F7: the reader list corrected (`textbuf.rs` out, `tour.rs` in). F8: hunk bytes reach disk
only through hunk flags; those are the byte invariants. F9: the expected cut stated, the
buffer presized. F10: reused rows leave `wanted`. F11: equivalence tests and per-fix
attribution runs. F12: `Accepted.pile` and accept-by-hunk on the clone list.

## Code review of Part B, folded (2026-10-26)

Fresh-context reviewer over `1b074c3..019a35b`, verdict "safe to push", no blocker or
major. It tried to make a reused row differ from a fresh render (baseline moves, renames,
the `Unread` and `Deleted` rows that share their sides, reloads, failed batches, the row
cap) and could not; a mutated reuse arm failed both equivalence tests. F1 (a pair of blobs
past 4 GiB could reach the `u32` offset's `expect` when `collapse_size_bytes` is raised
that far): such a pair now collapses as `Size`, `42a85d9`. F2 (the reuse-off scenario run
was wired nowhere): `just test-scenarios-no-reuse`, step 5 of `just test-prepush`,
`803b33d`. F3: below, for the sponsor. F4 to F6 (accept-by-hunk copies one row's hunks
because the §6 request types hold `Vec<Hunk>`; a dead `?`; a blob shared across two chunks
is fetched twice): no action.

## Part B results (2026-10-26)

Runs D (stride 150) and E (stride 5) at `803b33d`, scale 1.0, and one attribution run per
fix at stride 5 and scale 0.34; all in `docs/dev/bench.md` (S5, "Phase 17 Part B").

| | before (A, B / C) | after (D / E) |
|---|---|---|
| quiet end, footprint, the Activity Monitor number (MB) | 132 to 206 / 178 to 256 | **43 to 63 / 47 to 83** |
| same, scale 0.34, stride 5, cycles 1 / 2 / 3 (MB) | 340 / 731 / 1,113 | **51 / 50 / 43** |
| ramp end, live heap (MB) | 52 to 133 / 1,046 to 1,112 | **18 to 21 / 279 to 334** |
| churn peak RSS (MB) | 586 to 735 / 1,978 to 2,306 | **118 to 161 / 351 to 408** |
| relaunch peak RSS (MB) | 827, 835 / 1,508 | **767 / 986** |
| CPU while six worktrees write (%) | 27 / 32 | **4.8 / 4.4** |

The expectation (live heap at stride 5's ramp end roughly 300 to 400 MB) is met. Fix 3
carries most of the CPU and the churn peak, fixes 1 and 2 most of the live heap; fix 4
does not engage at S5's size (each root's batch is under one 64 MiB chunk). `just bench`
S1 to S4 (run I) are within noise of run H. What remains is the relaunch: up to eight
roots render whole piles at once, and macOS keeps that high-water mark (after run D's
relaunch, 18 MB live and 310 MB footprint).

**Judgment calls for the sponsor** (taken under the delegated-rulings practice, two-way
doors):
1. Fix 4 (blobs fetched in chunks) was added by the orchestrator after the design review
   (F5); it changes no output and did not engage in S5.
2. Code review F3: a scan in which every row is reused fetches nothing, so a store that
   broke since the last scan is reported at the first scan where some row moves, not at
   the next scan. Until then the screen shows the last good diffs instead of empty rows.
3. The reuse-off switch is a test-only variable, `LASTCALL_TEST_NO_DIFF_REUSE`, read in
   `env.rs` and applied only by the testkit; the binary never reads it, and it is in
   `docs/dev/testing.md`, not `docs/config.md`.

## Part B closing ruling (2026-10-30)

The sponsor, on the two questions put with the results: "1 rec 2 rec yeah It seems like you
already had a lot of good performance updates so far and I can see how it goes in real
workloads".

1. **The relaunch peak: try a smaller chunk, keep it only if the peak drops.** It dropped:
   `BLOB_CHUNK_BYTES` went from 64 MiB to 8 MiB. Relaunch peak RSS 767 to 213 MB (stride
   150) and 986 to 376 MB (stride 5); the footprint after the relaunch 310 to 31 MB and 581
   to 89 MB; S1 to S4's spawn counts identical (`docs/dev/bench.md`, runs J, K and L). The
   "Part B results" above describe the 64 MiB build: fix 4 now engages at S5's size, and the
   relaunch is no longer far above the rest of the run.
2. **Diffs on demand: held.** The sponsor watches real workloads on this build first. It
   reopens only if a real pile stays at several million pending lines; the trigger is what
   Activity Monitor shows there.
