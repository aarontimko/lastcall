# The performance baseline (`just bench`)

Baseline only; targets are set at the Phase 9 kickoff (§8). This page records what
`just bench` printed on one machine on one day so that a later run has something to be
compared against. It is not a gate: nothing here fails a build.

## How the numbers are taken

`just bench` builds the release binary and runs the five `#[ignore]`d scenarios of
`crates/lastcall/tests/test_bench.rs` (`cargo test --release -p lastcall --test test_bench
-- --ignored --nocapture --test-threads=1`). Each scenario prints one
`BENCH <scenario> <metric>=<value>` line per metric to stderr; the table below is those
lines, copied. A debug build prints a SKIP line and measures nothing.

- Fixtures are built outside every timed region: the 100 clones and their 4,000 edits, the
  100,000-line file, the 1,000- and 50,000-file drops. Each lives under a `TempDir` that its
  drop removes; nothing touches the real state dir or config (`LASTCALL_STATE_DIR`,
  `XDG_CONFIG_HOME` and `HOME` all point into the temp dir).
- In-process metrics come from an `Engine` opened over the same state dir the binary then
  runs on. `*_ms` is wall time; `*_spawns` is the `lastcall_engine::git::spawn_count` delta,
  i.e. how many git processes the region started.
- Screen metrics come from the built binary in the testkit's PTY harness (100×30). Every
  screen `*_ms` value has the harness's `POLL` granularity: the screen is read every 10 ms,
  so a value is "the first 10 ms tick at which the text was on screen".
- `peak_rss_kb` is the binary's resident set as `ps -o rss= -p <pid>` reports it, sampled
  by a harness thread from spawn to exit, maximum kept. The sampler sleeps `POLL` between
  samples, so its period is ≈10 ms plus one `ps` (about 2.5 ms on this machine), not a
  strict 10 ms tick. A `ps` that gives no number is skipped, not fatal; a sampler that gave
  up before the child exited prints a `PARTIAL` note next to the number. No `getrusage`; no
  new dependency.
- The TUI runs with `--poll 1` in S1, S2 and S4 (and in S3's first half), so head polling
  and the full rescan both tick every second; the `S3_events` half runs with the defaults
  (debounce 750 ms with a 3 s cap, head poll 10 s, rescan 30 s) so that the number is the watcher's, not
  the rescan backstop's. If the watcher never delivered the burst within 25 s the harness
  prints a SKIP line instead of a number.

## Machine

| | |
|---|---|
| CPU | Apple M1 Pro, 10 CPUs (`sysctl -n machdep.cpu.brand_string hw.ncpu`) |
| OS | macOS 15.7.4 (24G517) (`sw_vers`) |
| git | git version 2.37.1 (Apple Git-137.1) |
| rustc / cargo | rustc 1.98.0 (88d9e12ae 2026-08-18) / cargo 1.98.0 (797e8a9bc 2026-08-05) |
| profile | `[profile.release] strip = "debuginfo"` (otherwise cargo's defaults) |
| filesystem watcher | FSEvents, healthy: `S3_events` settled in the same time as `--poll 1` |
| date | 2026-09-02 |
| commit | the engine and TUI at `3579bf1` (the harness and this page are committed on top of it) |

## The baseline (run A)

### S1 — 100 clones, 40 edited files each (4,000 rows)

100 repos `r000`–`r099`, each with one committed 40-file tree and every file then edited
in the working tree; first sight taken before the edits. `open_ms` is `Engine::open` over
the 100 roots whose ledgers already exist (discovery, then per root: store open, HEAD
inspection, two `config --get`s, ledger load). `scan_all_ms` is one `Engine::scan_all`
over the 100 roots. The screen metrics run from spawning `lastcall tui --poll 1` to the
first root's pile (`1 repo · 40 files`) and to the header showing every root
(`100 repos · 4,000 files`); the RSS peak is read after two further one-second rescans.

| metric | value |
|---|---|
| `open_ms` | 33050 |
| `open_spawns` | 2902 |
| `roots` | 100 |
| `rows` | 4000 |
| `scan_all_ms` | 25553 |
| `scan_all_spawns` | 1900 |
| `first_pile_ms` | 33623 |
| `first_frame_ms` | 56568 |
| `peak_rss_kb` | 31936 |

What the numbers say about each other: 4,000 rows cost 1,900 spawns (19 per root, 13 ms
per spawn on average), and the TUI's time to the first pile (33.6 s) is the engine's open
(33.1 s, 29 spawns per root); the first full frame (56.6 s) is open plus one scan of every
root. Both phases are git-process bound on this machine, not diff bound.

### S2 — one 100,000-line file, every line changed but eleven

`big.txt` is 100,000 lines of `line NNNNN` (1,100,001 bytes), committed and first-sighted,
then rewritten as `LINE NNNNN` on every line except 50,000–50,010, which gives two hunks
under `collapse_size_bytes = 16777216` (the default is 524288, at which the file is a
collapsed row). `scan_ms` is one `Engine::scan` of the root. The screen metrics are `jj`+`enter`
to the first hunk header on screen (`open_ms`), `n` to the second hunk's header
(`hunk_next_ms`) and PageDown to a changed body (`page_down_ms`). `S2_default_config
open_ms` is the same file under the default config: `jj`+`enter` from the collapsed row
(`⊟`) to the `collapsed (…)` main pane.

| metric | value |
|---|---|
| `file_bytes` | 1100001 |
| `hunks` | 2 |
| `scan_ms` | 270 |
| `scan_spawns` | 19 |
| `open_ms` | 12 |
| `hunk_next_ms` | 15 |
| `page_down_ms` | 14 |
| `peak_rss_kb` | 56224 |
| `S2_default_config open_ms` | 12 |

The scan runs Myers twice per row — once for `counts` (the `+n −m` in the nav) and once
for `diff` (the hunks) — so the 100,000-line file is diffed twice in the 270 ms. A Phase 9
target candidate, noted here, not acted on. `scan_spawns` is **16** as of Phase 6 (run D
below); this row is the Phase 4 number.

### S3 — 1,000 files dropped into a clean root under watch

One first-sighted repo with the TUI showing `nothing pending across 1 root` and the status
line at `watching …`; then 1,000 files (`d00`–`d09`, 100 per dir) written as fast as the
harness can. `settle_ms` runs from the last write to `1 repo · 1,000 files` in the header.
`S3` is `tui --poll 1`; `S3_events` is `tui` with the default timings (debounce 750 ms under
its 3 s cap, the 30 s rescan backstop), so its number is the watcher's own.

| metric | `S3` (`--poll 1`) | `S3_events` (defaults) |
|---|---|---|
| `files` | 1000 | 1000 |
| `settle_ms` | 1553 | 1553 |
| `peak_rss_kb` | 10592 | 8832 |

### S4 — 50,000 files dropped into a clean root: the row cap

One first-sighted repo under `tui --poll 1`, then 50,000 files (`d00`–`d49`, 1,000 per
dir) written as fast as the harness can (4.7 s, outside the timed region).
`capped_count_ms` runs from the last write to `10,000+ files` in the header;
`settle_ms` to the row-cap notice with its final numbers in the root's main view
(`j` selects the root once it is listed):

```text
10,000 files shown · 40,000 more changed paths not scanned (first 10,000 by path)
```

The in-process `scan_ms` is one `Engine::scan` of the same root after the TUI has quit.

| metric | value |
|---|---|
| `capped_count_ms` | 3617 |
| `settle_ms` | 5342 |
| `peak_rss_kb` | 88448 |
| `scan_ms` | 1673 |
| `scan_spawns` | 18 |
| `rows_shown` | 10000 |
| `omitted` | 40000 |

`rows_shown == DEFAULT_ROW_CAP` and `omitted == 50,000 − cap` are asserted by the
harness, not just printed. `scan_spawns` is **15** as of Phase 6 (run D below); this row is
the Phase 4 number.

## Variance (run B, and a third run)

Run B, same machine, started right after run A. The bounds, from A/B and the third run
below: metrics of 100 ms and more agree to ≈2 % (S1 `open_ms` / `scan_all_ms` /
`first_frame_ms`, S2 `scan_ms`, S3 `settle_ms`, S4 `settle_ms` / `scan_ms`; the widest
A/B spread is S3 `settle_ms`, 1595 vs 1553, +2.7 %); the sub-20 ms screen metrics (S2
`open_ms`, `hunk_next_ms`, `page_down_ms`) are quantised to the 10 ms `POLL` tick, so 12 vs
15 ms is one tick, not a change; `peak_rss_kb` moves by up to ±15 % (S2 52848 vs 56224
between A and B, 12.8 % on the third run); every spawn, row and file count is identical.

The baseline is a two-run A/B. A third, independent run (the verifier's; same machine,
same commit) reproduced every count bit-identically and every headline number within
those bounds: S1 `open_ms` 32368 / `scan_all_ms` 25177 / `first_frame_ms` 55584; S2
`scan_ms` 268; S3 `settle_ms` 1575; S4 `scan_ms` 1662, `settle_ms` 5444, `rows_shown`
10000, `omitted` 40000.

Run B's raw lines:

```text
BENCH S1 open_ms=32945
BENCH S1 open_spawns=2902
BENCH S1 roots=100
BENCH S1 rows=4000
BENCH S1 scan_all_ms=25430
BENCH S1 scan_all_spawns=1900
BENCH S1 first_pile_ms=33647
BENCH S1 first_frame_ms=56583
BENCH S1 peak_rss_kb=31232
BENCH S2 file_bytes=1100001
BENCH S2 hunks=2
BENCH S2 scan_ms=274
BENCH S2 scan_spawns=19
BENCH S2 open_ms=15
BENCH S2 hunk_next_ms=15
BENCH S2 page_down_ms=14
BENCH S2 peak_rss_kb=52848
BENCH S2_default_config open_ms=15
BENCH S3 files=1000
BENCH S3 settle_ms=1595
BENCH S3 peak_rss_kb=10768
BENCH S3_events files=1000
BENCH S3_events settle_ms=1552
BENCH S3_events peak_rss_kb=8832
BENCH S4 capped_count_ms=3623
BENCH S4 settle_ms=5387
BENCH S4 peak_rss_kb=83360
BENCH S4 scan_ms=1687
BENCH S4 scan_spawns=18
BENCH S4 rows_shown=10000
BENCH S4 omitted=40000
```

## Phase 5 (run C): the batched reads and the bounded pool

`docs/spec/94-phase5-kickoff.md` deliverable 1. Three things changed between run A and
this one: one `config --list -z` and one batched `rev-parse` per root instead of seven
`--get`s and six separate `rev-parse`s (1c), roots opened and scanned on a bounded thread
pool of `min(available_parallelism(), 8)` (1a/1b), and the watcher's initial pass going
through `Engine::scan_all` instead of scanning one root at a time behind the engine mutex.

Two columns are measured, on the same machine, from the same commit, over freshly built
fixtures: **pool off** is the whole run with `LASTCALL_PARALLELISM=1`, which pins both the
engine's pool and the binary's to one thread and so isolates the pool from the batched
reads; **after** is the default width (8 on this machine). Run A's numbers are the Phase 4
baseline at commit `3579bf1`, kept for the columns that are comparable.

Machine block: unchanged from above except the date (2026-09-04) and the commit (this
branch, `eaad038` plus the watcher change). Command:

```text
cargo build --release -p lastcall
cargo test --release -p lastcall --test test_bench -- --ignored --nocapture --test-threads=1 bench_s1
```

### S1 — 100 clones, 40 edited files each (4,000 rows)

| metric | run A (Phase 4) | pool off (width 1) | after (width 8) |
|---|---|---|---|
| `open_ms` | 33050 | 12954 | 4228 |
| `open_spawns` | 2902 | 1102 | 1102 |
| `roots` | 100 | 100 | 100 |
| `rows` | 4000 | 4000 | 4000 |
| `scan_all_ms` | 25553 | 23801 | 4493 |
| `scan_all_spawns` | 1900 | 1700 | 1700 |
| `first_pile_ms` | 33623 | 13851 | 4803 |
| `first_frame_ms` | 56568 | 35947 | 8762 |
| `peak_rss_kb` | 31936 | 28288 | 28736 |

`scan_all_spawns` is **1600** as of Phase 6 (run D below); this table's 1700 is the Phase 5
number and the "before" that run D is measured against.

### S1h — 50 clones, 80 edited files each (4,000 rows)

New in Phase 5: the same 4,000 rows as S1 but half the roots and twice the files each, so
that the per-root cost and the per-row cost can be told apart. No run-A column exists; the
scenario did not.

| metric | pool off (width 1) | after (width 8) |
|---|---|---|
| `open_ms` | 6484 | 2179 |
| `open_spawns` | 552 | 552 |
| `roots` | 50 | 50 |
| `rows` | 4000 | 4000 |
| `scan_all_ms` | 13348 | 3437 |
| `scan_all_spawns` | 850 | 850 |
| `first_pile_ms` | 6977 | 2641 |
| `first_frame_ms` | 18512 | 4782 |
| `peak_rss_kb` | 23344 | 27968 |

### The raw `BENCH` lines

Width 8 (`cargo test --release … bench_s1`):

```text
BENCH S1 open_ms=4228
BENCH S1 open_spawns=1102
BENCH S1 roots=100
BENCH S1 rows=4000
BENCH S1 scan_all_ms=4493
BENCH S1 scan_all_spawns=1700
BENCH S1 first_pile_ms=4803
BENCH S1 first_frame_ms=8762
BENCH S1 peak_rss_kb=28736
BENCH S1h open_ms=2179
BENCH S1h open_spawns=552
BENCH S1h roots=50
BENCH S1h rows=4000
BENCH S1h scan_all_ms=3437
BENCH S1h scan_all_spawns=850
BENCH S1h first_pile_ms=2641
BENCH S1h first_frame_ms=4782
BENCH S1h peak_rss_kb=27968
```

Width 1 (`LASTCALL_PARALLELISM=1 cargo test --release … bench_s1`):

```text
BENCH S1 open_ms=12954
BENCH S1 open_spawns=1102
BENCH S1 roots=100
BENCH S1 rows=4000
BENCH S1 scan_all_ms=23801
BENCH S1 scan_all_spawns=1700
BENCH S1 first_pile_ms=13851
BENCH S1 first_frame_ms=35947
BENCH S1 peak_rss_kb=28288
BENCH S1h open_ms=6484
BENCH S1h open_spawns=552
BENCH S1h roots=50
BENCH S1h rows=4000
BENCH S1h scan_all_ms=13348
BENCH S1h scan_all_spawns=850
BENCH S1h first_pile_ms=6977
BENCH S1h first_frame_ms=18512
BENCH S1h peak_rss_kb=23344
```

### Re-measured at the end of the phase

The table above was taken right after deliverable 1. Deliverables 2 and 3 landed after it
(a `read_dir` of the repo dir added to every `Store::open` for the stale-temp-index sweep,
among other things), so the two scenarios were run again at the branch tip on the same
machine:

```text
BENCH S1 open_ms=4291        BENCH S1h open_ms=2202
BENCH S1 open_spawns=1102    BENCH S1h open_spawns=552
BENCH S1 roots=100           BENCH S1h roots=50
BENCH S1 rows=4000           BENCH S1h rows=4000
BENCH S1 scan_all_ms=4464    BENCH S1h scan_all_ms=3113
BENCH S1 scan_all_spawns=1700 BENCH S1h scan_all_spawns=850
BENCH S1 first_pile_ms=4900  BENCH S1h first_pile_ms=2588
BENCH S1 first_frame_ms=8803 BENCH S1h first_frame_ms=4845
BENCH S1 peak_rss_kb=29808   BENCH S1h peak_rss_kb=28032
```

Every figure is inside run-to-run noise of the table (`first_frame_ms` 8762 → 8803 and
4782 → 4845, under 1 %), spawn counts identical, and both ceilings still met. The sweep's
one extra `read_dir` per root at open does not show.

### What changed, in one paragraph

The batched reads alone (the width-1 column against run A) take S1's open from 33.1 s to
13.0 s: **29 git processes per root at open became 11** — nine in `open_root` plus two in
discovery — and the head inspection inside a scan going from six spawns to four took the
**per-scan count from 19 per root to 17**. Time per spawn is unchanged, so the open is
still git-process bound; there are simply fewer processes. The pool is the rest: at width
8, open falls 12.95 s → 4.23 s and `scan_all` 23.80 s → 4.49 s, both close to the ×5–6 an
8-wide pool of I/O-bound children reaches on ten cores. The user-visible number,
`first_frame_ms`, is **56568 → 8762** on S1 (6.5×, against the kickoff's ≥ 4× and its
14,142 ms ceiling) and **4782 on S1h** (ceiling 6,000). Two thirds of the S1 gain came from
the pool and one third from the batching, and the last piece was the watcher: its initial
pass used to scan one root at a time behind the engine mutex, which the pool cannot help,
so the gap between the first pile and the full frame stayed at ~22 s until that pass became
one `scan_all` (S1's first pile is still streamed on its own, so `first_pile_ms` is
unaffected). `peak_rss_kb` moves by less than the sampler's noise: eight concurrent roots
cost about 0.4 MB over one on S1, and nothing near a per-root allocation.

## Phase 6 (run D): one remote-ref listing per scan

`docs/spec/95-phase6-kickoff.md` deliverable 5. Classification used to run
`for-each-ref refs/remotes` to build its memo key and then `classify` ran the listing
again; now the listing that builds the key is the listing `classify` is given, so a scan
issues **one** `for-each-ref`, not two. Nothing else in this phase touches the scan's git
usage (`hunks_of` is on-demand, off every scan by construction).

Machine block: unchanged from above except the date (2026-09-05) and the commit (this
branch's tip). Command: `just bench` (all five scenarios, `--test-threads=1`), one run.

### The spawn rows

| scenario | metric | before | after | per root per scan |
|---|---|---|---|---|
| S1 | `open_spawns` | 1102 | 1102 | 11 → 11 (open lists no remote refs) |
| S1 | `scan_all_spawns` | 1700 | **1600** | 17 → **16** |
| S2 | `scan_spawns` | 19 | **16** | one root, one scan |
| S4 | `scan_spawns` | 18 | **15** | one root, one scan |

S1 is the clean comparison: its "before" is the Phase 5 measurement in the table above, on
the same fixture, so the whole −100 is this phase — exactly one fewer git process per root
per scan, across 100 roots. S2's and S4's "before" columns are the **Phase 4** run-A
numbers (neither scenario was re-run in Phase 5), so their −3 carries Phase 5's batched
head inspection as well as this phase's −1; the Phase 5 paragraph's "19 per scan became 17"
is the missing middle. No count moved that was not a spawn: `rows`, `hunks`, `rows_shown`
and `omitted` are identical to run A.

Wall times are lower across the board against run A, but run A predates the bounded pool
and the batched reads and is not a like-for-like: read them against the Phase 5 table
instead (S1 `open_ms` 4291 → 3682, `scan_all_ms` 4464 → 3853, `first_frame_ms` 8803 →
7533 — the spawn the scan no longer makes, times 100 roots, plus run-to-run noise).

### The raw `BENCH` lines

```text
BENCH S1 open_ms=3682
BENCH S1 open_spawns=1102
BENCH S1 roots=100
BENCH S1 rows=4000
BENCH S1 scan_all_ms=3853
BENCH S1 scan_all_spawns=1600
BENCH S1 first_pile_ms=4370
BENCH S1 first_frame_ms=7533
BENCH S1 peak_rss_kb=21728
BENCH S1h open_ms=2093
BENCH S1h open_spawns=552
BENCH S1h roots=50
BENCH S1h rows=4000
BENCH S1h scan_all_ms=2999
BENCH S1h scan_all_spawns=800
BENCH S1h first_pile_ms=2226
BENCH S1h first_frame_ms=3994
BENCH S1h peak_rss_kb=21280
BENCH S2 file_bytes=1100001
BENCH S2 hunks=2
BENCH S2 scan_ms=206
BENCH S2 scan_spawns=16
BENCH S2 open_ms=13
BENCH S2 hunk_next_ms=13
BENCH S2 page_down_ms=14
BENCH S2 peak_rss_kb=53680
BENCH S2_default_config open_ms=15
BENCH S3 files=1000
BENCH S3 settle_ms=1539
BENCH S3 peak_rss_kb=9792
BENCH S3_events files=1000
BENCH S3_events settle_ms=1528
BENCH S3_events peak_rss_kb=9632
BENCH S4 capped_count_ms=3485
BENCH S4 settle_ms=5123
BENCH S4 peak_rss_kb=78608
BENCH S4 scan_ms=1540
BENCH S4 scan_spawns=15
BENCH S4 rows_shown=10000
BENCH S4 omitted=40000
```

S2's screen half (`open_ms` and below) is from a second invocation of `bench_s2` alone: the
first `just bench` failed there on an assertion that predates this phase. `bench_s2` waited
for the nav to read `M big.txt`, but `+99,989 −99,989` with thousands separators (`cd045b2`,
Phase 4 ruling 2) leaves the 28-column nav no room for the extension, so the row renders
`M big.…`. The scenario's own numbers were never wrong — the wait was — and the assertion
is now `M big`. S1, S3 and S4 were unaffected and their lines are from the single full run.

## What the first run found

The first full run hung in S4: with more than about 4,000 paths in one batch, the engine's
`git hash-object -w --stdin-paths` never returned. `run_command` in
`crates/lastcall-engine/src/git.rs` wrote the whole stdin before reading stdout, so once
the child had filled its stdout pipe with object ids it blocked on write while we blocked
on writing its stdin. The fix (`3579bf1`) feeds stdin from a scoped thread while
`wait_with_output` drains; the regression unit test is
`git_run_command_feeds_stdin_while_draining_stdout` (300 KiB through `cat`). The S4
numbers above are from after the fix; the row cap was unreachable before it.

## Re-running

`just bench` takes about four minutes (the S1 fixture alone is 100 clones × 40 files,
80 s of git). Run it on a quiet machine, twice; record the second run's spread in one line
under "Variance". Keep the raw `BENCH` lines in the PR description, copy them here
verbatim, and update the machine block if anything in it changed. Do not add targets to
this page.
