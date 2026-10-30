# The performance baseline (`just bench`)

Baseline only; targets are set at the Phase 9 kickoff (§8). This page records what
`just bench` printed on one machine on one day so that a later run has something to be
compared against. It is not a gate: nothing here fails a build.

## How the numbers are taken

`just bench` builds the release binary and runs the five `#[ignore]`d scenarios of
`crates/lastcall/tests/test_bench.rs` (`cargo test --release -p lastcall --test test_bench
-- --ignored --nocapture --test-threads=1`). Each scenario prints one
`BENCH <scenario> <metric>=<value>` line per metric to stderr; the table below is those
lines, copied. A debug build prints a SKIP line and measures nothing. S5, the worktree
soak, is skipped by `just bench` and runs alone under `just bench-soak` (its own section,
"Phase 17 (S5)", below).

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

> **`first_pile_ms` changed meaning at the Gate 8 sponsor run (`9d2fd4a`, 2026-09-07).**
> The TUI no longer lists a root until every root has reported (the launch hold,
> `tui.md` "Startup and the first frame"), so `1 repo · 40 files` is never on screen; the
> harness now waits for the hold's counter line to show a first root reported
> (`K of 100 repos checked`, `K` ≥ 1). The counter appears one second into the hold, so
> from that commit on `first_pile_ms` **floors at ~1,000 ms** and reads "the first root was
> done by then", not "the first root was listed then". `first_frame_ms` keeps its meaning
> (every root in the header) and is the number to compare across runs. Runs A–F below were
> measured under the old meaning.
>
> **Renamed `first_checked_ms` in the harness, forward only (2026-09-11, Phase 9b
> deliverable 10, ruling P7).** The name now says what the number has measured since
> `9d2fd4a`: the first root was *checked* by then. The rename is forward only — runs A–F's
> tables and raw `BENCH` lines below keep the name they were printed under, because
> rewriting them would claim measurements that were never taken. Run G and every run after
> it print `first_checked_ms`. The gate targets for it live in the Phase 9b kickoff's perf
> table, not on this page.
>
> **The "floors at ~1,000 ms" sentence above was a prediction, and run G disproved it.**
> Nothing had run the counter-only wait under a bench between `9d2fd4a` and run G: run F was
> taken the day before that commit, under the old meaning. Run G's first attempt timed out
> in S1 and S1h with the complete listing on screen. The TUI is spawned after the scenario
> has already opened and scanned the same roots in process, so its own scan runs warm, and on
> a quiet machine it finishes **before** the hold's one-second counter is ever drawn: the
> line the harness was waiting for does not exist on a fast run. The wait now takes either
> form — the counter line, or the hold already over with every root in the header, which is
> the same fact stated more strongly. So `first_checked_ms` floors at ~1,000 ms only on a run
> slow enough to show the counter; on a quiet warm run it equals `first_frame_ms`, and both
> readings mean "the first root was checked by then".

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

**Run G's spread (2026-09-11, the quiet-machine two-run below).** Two back-to-back runs on
an idle machine: every count bit-identical; every wall metric over 100 ms within ±5.4 % run
to run except **S1h `scan_all_ms`, 3728 vs 3143 (−15.7 %)**, which is wider than anything
A/B saw and is the one number on this page that should not be read to ±2 %; the sub-20 ms
screen metrics moved by one 10 ms tick as ever; `peak_rss_kb` held to ±1 % on S1/S1h and
±9 % on S3/S4 but moved **32.8 % on S2** (45728 vs 60704), so its ±15 % band is a claim
about the other scenarios, not about S2.

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
one `scan_all` (S1's first pile was still streamed on its own at this point, so
`first_pile_ms` was unaffected; the Gate 8 sponsor run folded that solo scan into the
batch — see the note under S1). `peak_rss_kb` moves by less than the sampler's noise: eight concurrent roots
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

## Phase 7 (run E): restore and flag are off the scan path

`docs/spec/96-phase7-kickoff.md`. Phase 7 adds two engine seams (`Engine::restore`,
`Engine::flag`/`unflag`) and the TUI half that reaches them. Both are **on-demand**: they
run when a key is pressed, never inside a scan or a draw, and neither adds a git process to
the scan the way `hunks_of` does not. This run exists to show that — the counts are the
evidence, not the milliseconds.

Machine block: unchanged from above except the date (2026-09-05) and the commit (this
branch's tip). Command: `just bench` (all five scenarios, `--test-threads=1`), **one** run,
so read it against run D's counts rather than as a new baseline.

### The counts did not move

| scenario | metric | run D (before) | run E (after) |
|---|---|---|---|
| S1 | `open_spawns` | 1102 | 1102 |
| S1 | `scan_all_spawns` | 1600 | 1600 |
| S1 | `rows` / `roots` | 4000 / 100 | 4000 / 100 |
| S1h | `open_spawns` / `scan_all_spawns` | 552 / 800 | 552 / 800 |
| S2 | `scan_spawns` / `hunks` | 16 / 2 | 16 / 2 |
| S3 | `files` | 1000 | 1000 |
| S4 | `scan_spawns` | 15 | 15 |
| S4 | `rows_shown` / `omitted` | 10000 / 40000 | 10000 / 40000 |

Every spawn, row and file count is bit-identical to run D. That is the claim this run is
here to support: a phase that writes the working tree and the ledger from a keystroke costs
the scan nothing.

### Wall times, with a caveat

| scenario | metric | run D | run E | Δ |
|---|---|---|---|---|
| S1 | `open_ms` | 3682 | 4142 | +12.5 % |
| S1 | `scan_all_ms` | 3853 | 3991 | +3.6 % |
| S1 | `first_frame_ms` | 7533 | 8319 | +10.4 % |
| S1h | `scan_all_ms` | 2999 | 3183 | +6.1 % |
| S1h | `first_frame_ms` | 3994 | 4429 | +10.9 % |
| S2 | `scan_ms` | 206 | 237 | +15.0 % |
| S3 | `settle_ms` | 1539 | 1566 | +1.8 % |
| S3_events | `settle_ms` | 1528 | 1543 | +1.0 % |
| S4 | `scan_ms` | 1540 | 1669 | +8.4 % |
| S4 | `settle_ms` | 5123 | 5344 | +4.3 % |

These are **above** the ±2 % band the Variance section records for metrics over 100 ms, and
this run is a single run on a machine that was not idle — the same workstation was running
the phase's own builds and test tiers. Two things say the difference is load and not code.
First, the counts above are identical, so no scenario is doing more work. Second, the
spread tracks how git-heavy a scenario is: S3, which spawns no git per file, is unchanged
(+1.0 % / +1.8 %), while the process-spawn-heavy S1 and S2 move most — process creation is
what a busy machine slows down. `peak_rss_kb` moves both ways within its ±15 % band (S2
53680 → 47392, S4 78608 → 85632).

The honest reading: this run establishes that Phase 7 changed no counts. It does not
re-establish the wall-time baseline. The next phase that wants one should follow the
"Re-running" note below — quiet machine, two runs — and compare against run D's times, not
these.

### The raw `BENCH` lines

```text
BENCH S1 open_ms=4142
BENCH S1 open_spawns=1102
BENCH S1 roots=100
BENCH S1 rows=4000
BENCH S1 scan_all_ms=3991
BENCH S1 scan_all_spawns=1600
BENCH S1 first_pile_ms=4744
BENCH S1 first_frame_ms=8319
BENCH S1 peak_rss_kb=20448
BENCH S1h open_ms=2160
BENCH S1h open_spawns=552
BENCH S1h roots=50
BENCH S1h rows=4000
BENCH S1h scan_all_ms=3183
BENCH S1h scan_all_spawns=800
BENCH S1h first_pile_ms=2465
BENCH S1h first_frame_ms=4429
BENCH S1h peak_rss_kb=19696
BENCH S2 file_bytes=1100001
BENCH S2 hunks=2
BENCH S2 scan_ms=237
BENCH S2 scan_spawns=16
BENCH S2 open_ms=15
BENCH S2 hunk_next_ms=15
BENCH S2 page_down_ms=13
BENCH S2 peak_rss_kb=47392
BENCH S2_default_config open_ms=15
BENCH S3 files=1000
BENCH S3 settle_ms=1566
BENCH S3 peak_rss_kb=10592
BENCH S3_events files=1000
BENCH S3_events settle_ms=1543
BENCH S3_events peak_rss_kb=9168
BENCH S4 capped_count_ms=3609
BENCH S4 settle_ms=5344
BENCH S4 peak_rss_kb=85632
BENCH S4 scan_ms=1669
BENCH S4 scan_spawns=15
BENCH S4 rows_shown=10000
BENCH S4 omitted=40000
```

All five scenarios passed in one invocation (`5 passed; 0 failed`, 111 s of measured time
after a 55 s fixture build): unlike run D, S2's assertion needed no second pass.

## Phase 8 (run F): editing costs the scan nothing

`docs/spec/97-phase8-kickoff.md`. Phase 8 adds `Engine::read_rendered`, `Engine::save`, the
inline editor, the `$EDITOR` suspend and select-to-copy. Every one of them runs **from a
keystroke** — none is on the scan path, none is in a draw, and `save` adds no git process to
a scan (its own `hash-object` is paid once, by the save). Run E's claim, repeated for the
same reason: the counts are the evidence.

Machine block: unchanged from above except the date (**2026-09-06**) and the commit (this
branch's tip, `334ba73`). Command: `just bench`, all five scenarios, `--test-threads=1`,
**one** run. `5 passed; 0 failed`, 120.65 s of measured time.

### The counts did not move

| scenario | metric | run D | run E | run F |
|---|---|---|---|---|
| S1 | `open_spawns` | 1102 | 1102 | **1102** |
| S1 | `scan_all_spawns` | 1600 | 1600 | **1600** |
| S1 | `rows` / `roots` | 4000 / 100 | 4000 / 100 | **4000 / 100** |
| S1h | `open_spawns` / `scan_all_spawns` | 552 / 800 | 552 / 800 | **552 / 800** |
| S2 | `scan_spawns` / `hunks` | 16 / 2 | 16 / 2 | **16 / 2** |
| S3 | `files` | 1000 | 1000 | **1000** |
| S4 | `scan_spawns` | 15 | 15 | **15** |
| S4 | `rows_shown` / `omitted` | 10000 / 40000 | 10000 / 40000 | **10000 / 40000** |

Bit-identical to run D and run E. A phase that writes the working tree, the ledger and the
user's clipboard from a keystroke still costs the scan nothing.

### Wall times

Read against **run D**, per run E's own instruction (run E did not re-establish the
baseline). The band the Variance section records is ±2 % for metrics over 100 ms.

| scenario | metric | run D | run E | run F | Δ vs D |
|---|---|---|---|---|---|
| S1 | `open_ms` | 3682 | 4142 | 4724 | +28.3 % |
| S1 | `scan_all_ms` | 3853 | 3991 | 4402 | +14.2 % |
| S1 | `first_pile_ms` | 4370 | 4744 | 5173 | +18.4 % |
| S1 | `first_frame_ms` | 7533 | 8319 | 9150 | +21.5 % |
| S1h | `scan_all_ms` | 2999 | 3183 | 3384 | +12.8 % |
| S1h | `first_frame_ms` | 3994 | 4429 | 4877 | +22.1 % |
| S2 | `scan_ms` | 206 | 237 | 262 | +27.2 % |
| S2 | `open_ms` | 13 | 15 | 12 | −7.7 % |
| S2 | `hunk_next_ms` | 13 | 15 | 10 | −23.1 % |
| S3 | `settle_ms` | 1539 | 1566 | 1546 | +0.5 % |
| S3_events | `settle_ms` | 1528 | 1543 | 1567 | +2.6 % |
| S4 | `scan_ms` | 1540 | 1669 | 1746 | +13.4 % |
| S4 | `settle_ms` | 5123 | 5344 | 5930 | +15.8 % |

**No row moved beyond noise for a reason this phase can be held to, and one row is worth
naming.** The wall times are up on run D by the same *pattern* run E recorded and for the
same reason: this is a single run on a workstation that was building and testing the phase
at the same time, not a quiet machine. The evidence that it is load and not code is the
same two facts. First, every count above is identical, so no scenario is doing more work.
Second, the spread still tracks how process-heavy a scenario is — S3, which spawns no git
per file, is inside the band at +0.5 % / +2.6 %, while the spawn-heavy S1, S2 and S4 move
most, and process creation is what a busy machine slows down. The in-process S2 interactions
that Phase 8 might plausibly have touched went the *other* way — `open_ms` 13 → 12,
`hunk_next_ms` 13 → 10, `page_down_ms` 14 → 12 against run D — which no amount of load
explains as an improvement and which is consistent with "the diff pane's code did not get
slower". They are also 10–15 ms values with a 10 ms harness granularity, so they are worth
no more than that. `peak_rss_kb` moves both ways inside its ±15 % band (S1 21728 → 21664,
S2 53680 → 49152, S4 78608 → 86368 against run D).

The honest reading, again: **run F establishes that Phase 8 changed no counts. It does not
re-establish the wall-time baseline, and neither did run E.** Two runs on a quiet machine are
still owed; the phase that needs the number should take them and compare against run D.

Nothing in the bench measures the editor, the save or the clipboard: `test_bench.rs` gained
no scenario. That is deliberate — none of the three is on a path a scan or a frame takes, and
a scenario that pressed `i` would be measuring the PTY harness. The one cost Phase 8 *does*
add to a launch is not a scan cost at all, and it has its own section below.

### The raw `BENCH` lines

```text
BENCH S1 open_ms=4724
BENCH S1 open_spawns=1102
BENCH S1 roots=100
BENCH S1 rows=4000
BENCH S1 scan_all_ms=4402
BENCH S1 scan_all_spawns=1600
BENCH S1 first_pile_ms=5173
BENCH S1 first_frame_ms=9150
BENCH S1 peak_rss_kb=21664
BENCH S1h open_ms=2335
BENCH S1h open_spawns=552
BENCH S1h roots=50
BENCH S1h rows=4000
BENCH S1h scan_all_ms=3384
BENCH S1h scan_all_spawns=800
BENCH S1h first_pile_ms=2784
BENCH S1h first_frame_ms=4877
BENCH S1h peak_rss_kb=20192
BENCH S2 file_bytes=1100001
BENCH S2 hunks=2
BENCH S2 scan_ms=262
BENCH S2 scan_spawns=16
BENCH S2 open_ms=12
BENCH S2 hunk_next_ms=10
BENCH S2 page_down_ms=12
BENCH S2 peak_rss_kb=49152
BENCH S2_default_config open_ms=12
BENCH S3 files=1000
BENCH S3 settle_ms=1546
BENCH S3 peak_rss_kb=11168
BENCH S3_events files=1000
BENCH S3_events settle_ms=1567
BENCH S3_events peak_rss_kb=10416
BENCH S4 capped_count_ms=4106
BENCH S4 settle_ms=5930
BENCH S4 peak_rss_kb=86368
BENCH S4 scan_ms=1746
BENCH S4 scan_spawns=15
BENCH S4 rows_shown=10000
BENCH S4 omitted=40000
```

## Phase 9b (run G): the quiet-machine two-run

`docs/spec/99-phase9b-release-kickoff.md` deliverable 10, ruling P7. Runs E and F were each a
single run on a workstation that was building and testing the phase at the same time, and
both said the same thing about themselves: two runs on a quiet machine are still owed. This
is that pair. Nothing else ran on the machine during either run, and the two ran back to
back.

Machine block: unchanged from above except the date (**2026-09-11**) and the commit (this
branch at `df88495`, plus this commit's harness fix). Command: `just bench`, all five
scenarios, `--test-threads=1`, twice. `5 passed; 0 failed` both times, **102.31 s** and
**102.85 s** of measured time, against ~120 s for the busy-machine runs E and F.

### The harness needed a fix before the run could finish

Run G's first attempt failed in S1 and S1h, both on the same wait, with the complete listing
already on screen. `first_checked_ms` waits for the launch hold's counter line (`K of 100
repos checked`), which the pane draws from one second into the hold. The wait was written
with the hold at `9d2fd4a` on 2026-09-07 and no bench had run it since: run F was taken on
2026-09-06, under the metric's old meaning. On a quiet machine there is nothing to wait for.
The scenario opens and scans all 100 roots in process before it spawns the TUI, so the TUI's
own scan runs warm and the hold ends **before** its counter is ever drawn.

The wait now takes either form: the counter line, or the hold already over with every root in
the header. The second is the stronger statement of the same fact (every root checked means
the first one was), so the metric's meaning is unchanged and the harness no longer depends on
the run being slow. It shows in the numbers below as `first_checked_ms` **equal to**
`first_frame_ms` in both runs, to the millisecond: one frame satisfied both waits.

### The counts did not move

| scenario | metric | run D | run F | run G ×2 |
|---|---|---|---|---|
| S1 | `open_spawns` | 1102 | 1102 | **1102 / 1102** |
| S1 | `scan_all_spawns` | 1600 | 1600 | **1600 / 1600** |
| S1 | `rows` / `roots` | 4000 / 100 | 4000 / 100 | **4000 / 100** (both) |
| S1h | `open_spawns` / `scan_all_spawns` | 552 / 800 | 552 / 800 | **552 / 800** (both) |
| S2 | `scan_spawns` / `hunks` | 16 / 2 | 16 / 2 | **16 / 2** (both) |
| S3 | `files` | 1000 | 1000 | **1000** (both) |
| S4 | `scan_spawns` | 15 | 15 | **15** (both) |
| S4 | `rows_shown` / `omitted` | 10000 / 40000 | 10000 / 40000 | **10000 / 40000** (both) |

Bit-identical to runs D, E and F, and to each other. Phase 9a and 9b are a TUI phase and a
release phase: nothing they added is on a scan path, and the counts are the evidence.

### Wall times

Read against **run D**, the last run on a comparably unloaded machine and the reference runs
E and F used. `Δ` is run G's two-run mean against run D.

| scenario | metric | run D | run F | **run G #1** | **run G #2** | Δ vs D |
|---|---|---|---|---|---|---|
| S1 | `open_ms` | 3682 | 4724 | **3704** | **3641** | −0.3 % |
| S1 | `scan_all_ms` | 3853 | 4402 | **4085** | **4018** | +5.2 % |
| S1 | `first_checked_ms` | 4370 (old meaning) | 5173 (old meaning) | **7339** | **7308** | n/a |
| S1 | `first_frame_ms` | 7533 | 9150 | **7339** | **7308** | −2.8 % |
| S1h | `open_ms` | 2093 | 2335 | **1881** | **1903** | −9.6 % |
| S1h | `scan_all_ms` | 2999 | 3384 | **3728** | **3143** | +14.6 % |
| S1h | `first_frame_ms` | 3994 | 4877 | **3930** | **4064** | +0.1 % |
| S2 | `scan_ms` | 206 | 262 | **211** | **209** | +1.9 % |
| S3 | `settle_ms` | 1539 | 1546 | **1522** | **1508** | −1.6 % |
| S3_events | `settle_ms` | 1528 | 1567 | **1513** | **1543** | ±0.0 % |
| S4 | `capped_count_ms` | 3485 | 4106 | **3690** | **3795** | +7.4 % |
| S4 | `scan_ms` | 1540 | 1746 | **1583** | **1664** | +5.4 % |
| S4 | `settle_ms` | 5123 | 5930 | **5314** | **5461** | +5.2 % |

**Run G re-establishes the wall-time baseline, and it lands on run D.** Every row that runs
E and F reported as up by 10–28 % is back: S1 `first_frame_ms` 9150 → 7323 (mean), S2
`scan_ms` 262 → 210, S4 `settle_ms` 5930 → 5388. That is the conclusion runs E and F both
predicted and neither could prove — the drift was the machine, not the code.

Two rows are worth naming rather than averaging away. **S1h `scan_all_ms`** is +14.6 % on the
mean, but the two quiet runs are themselves 15.7 % apart (3728 vs 3143), so the mean is the
weakest number in the table and the honest reading is "S1h's scan is noisy at ±16 %, and
3143 is inside run D's neighbourhood". Nothing else in S1h moved: its `first_frame_ms` is
+0.1 % and its spawn counts are identical. **`first_checked_ms` has no Δ** because runs D and
F printed it under the old meaning (the first root *listed*, before the launch hold existed);
comparing 4370 with 7339 would be comparing two different measurements, and the note under S1
says so.

`peak_rss_kb`: S1 19472 / 19408 and S1h 19552 / 19744 are the lowest this page has recorded
(run D: 21728 / 21280). S3 and S4 move ±9 % between the two runs. S2 moves 45728 → 60704,
32.8 % — the peak is sampled on a timer by the harness's RSS poller, and the scenario it
samples reads a 1.1 MB file and diffs 100,000 lines in one burst, so which tick catches the
burst decides the number. Read S2's RSS as "tens of megabytes", not as a figure.

### The raw `BENCH` lines

Run G #1:

```text
BENCH S1 open_ms=3704
BENCH S1 open_spawns=1102
BENCH S1 roots=100
BENCH S1 rows=4000
BENCH S1 scan_all_ms=4085
BENCH S1 scan_all_spawns=1600
BENCH S1 first_checked_ms=7339
BENCH S1 first_frame_ms=7339
BENCH S1 peak_rss_kb=19472
BENCH S1h open_ms=1881
BENCH S1h open_spawns=552
BENCH S1h roots=50
BENCH S1h rows=4000
BENCH S1h scan_all_ms=3728
BENCH S1h scan_all_spawns=800
BENCH S1h first_checked_ms=3930
BENCH S1h first_frame_ms=3930
BENCH S1h peak_rss_kb=19552
BENCH S2 file_bytes=1100001
BENCH S2 hunks=2
BENCH S2 scan_ms=211
BENCH S2 scan_spawns=16
BENCH S2 open_ms=15
BENCH S2 hunk_next_ms=15
BENCH S2 page_down_ms=15
BENCH S2 peak_rss_kb=45728
BENCH S2_default_config open_ms=11
BENCH S3 files=1000
BENCH S3 settle_ms=1522
BENCH S3 peak_rss_kb=10096
BENCH S3_events files=1000
BENCH S3_events settle_ms=1513
BENCH S3_events peak_rss_kb=9456
BENCH S4 capped_count_ms=3690
BENCH S4 settle_ms=5314
BENCH S4 peak_rss_kb=78480
BENCH S4 scan_ms=1583
BENCH S4 scan_spawns=15
BENCH S4 rows_shown=10000
BENCH S4 omitted=40000
```

Run G #2:

```text
BENCH S1 open_ms=3641
BENCH S1 open_spawns=1102
BENCH S1 roots=100
BENCH S1 rows=4000
BENCH S1 scan_all_ms=4018
BENCH S1 scan_all_spawns=1600
BENCH S1 first_checked_ms=7308
BENCH S1 first_frame_ms=7308
BENCH S1 peak_rss_kb=19408
BENCH S1h open_ms=1903
BENCH S1h open_spawns=552
BENCH S1h roots=50
BENCH S1h rows=4000
BENCH S1h scan_all_ms=3143
BENCH S1h scan_all_spawns=800
BENCH S1h first_checked_ms=4064
BENCH S1h first_frame_ms=4064
BENCH S1h peak_rss_kb=19744
BENCH S2 file_bytes=1100001
BENCH S2 hunks=2
BENCH S2 scan_ms=209
BENCH S2 scan_spawns=16
BENCH S2 open_ms=15
BENCH S2 hunk_next_ms=10
BENCH S2 page_down_ms=13
BENCH S2 peak_rss_kb=60704
BENCH S2_default_config open_ms=10
BENCH S3 files=1000
BENCH S3 settle_ms=1508
BENCH S3 peak_rss_kb=11024
BENCH S3_events files=1000
BENCH S3_events settle_ms=1543
BENCH S3_events peak_rss_kb=9472
BENCH S4 capped_count_ms=3795
BENCH S4 settle_ms=5461
BENCH S4 peak_rss_kb=85296
BENCH S4 scan_ms=1664
BENCH S4 scan_spawns=15
BENCH S4 rows_shown=10000
BENCH S4 omitted=40000
```

## Phase 11 (run H): the per-branch record costs no git process

One run, taken after the branch-switch engine change landed (Amendment v1.12). The change
adds one `HEAD` file read per scan and per head inspection, made with `std::fs::read` and no
git process, so the instrument that matters here is the spawn counts.

Machine block: unchanged except the date (**2026-09-15**) and the commit (this phase's branch
at `e6ba9cc`). Command: `just bench`, all five scenarios, `--test-threads=1`, once.
`5 passed; 0 failed`, **115.95 s** of measured time. The machine was not otherwise idle: an
editor and a long-running assistant session were open, which is the condition runs E and F
were taken under and run G was not.

### The counts did not move

| scenario | metric | run D | run G ×2 | run H |
|---|---|---|---|---|
| S1 | `open_spawns` | 1102 | 1102 / 1102 | **1102** |
| S1 | `scan_all_spawns` | 1600 | 1600 / 1600 | **1600** |
| S1 | `rows` / `roots` | 4000 / 100 | 4000 / 100 | **4000 / 100** |
| S1h | `open_spawns` / `scan_all_spawns` | 552 / 800 | 552 / 800 | **552 / 800** |
| S2 | `scan_spawns` / `hunks` | 16 / 2 | 16 / 2 | **16 / 2** |
| S3 | `files` | 1000 | 1000 | **1000** |
| S4 | `scan_spawns` | 15 | 15 | **15** |
| S4 | `rows_shown` / `omitted` | 10000 / 40000 | 10000 / 40000 | **10000 / 40000** |

Bit-identical to every run since D. The branch sync reads `<git_dir>/HEAD` directly and the
switch itself only runs when the name changed, which no bench scenario does: none of them
checks a branch out.

### Wall times

Read against run G's two-run mean, the quiet-machine baseline.

| scenario | metric | run G mean | run H | Δ |
|---|---|---|---|---|
| S1 | `open_ms` | 3673 | **4357** | +18.6 % |
| S1 | `scan_all_ms` | 4052 | **4444** | +9.7 % |
| S1 | `first_frame_ms` | 7324 | **9086** | +24.1 % |
| S1h | `open_ms` | 1892 | **2273** | +20.1 % |
| S1h | `scan_all_ms` | 3436 | **3040** | −11.5 % |
| S1h | `first_frame_ms` | 3997 | **4640** | +16.1 % |
| S2 | `scan_ms` | 210 | **248** | +18.1 % |
| S3 | `settle_ms` | 1515 | **1559** | +2.9 % |
| S3_events | `settle_ms` | 1528 | **1569** | +2.7 % |
| S4 | `capped_count_ms` | 3743 | **3831** | +2.4 % |
| S4 | `scan_ms` | 1624 | **1652** | +1.7 % |
| S4 | `settle_ms` | 5388 | **5591** | +3.8 % |

The shape is runs E and F's, not a new cost: the scenarios that spend their time launching
and scanning many roots at once (S1, S1h, S2) are 10 to 24 % up, while the ones whose number
is a watcher settling or a capped scan (S3, S4) are within 4 %. S1h `scan_all_ms` is *down*
11.5 %, which a real per-scan cost could not produce. Run G's own two quiet runs were 15.7 %
apart on that row. The counts above are the reason to read this table as the machine rather
than the change; a quiet-machine pair would settle it, and run G's note already says that is
the honest way to take wall times on this page.

`peak_rss_kb`: S1 19824 and S1h 18240, beside run G's 19472 / 19408 and 19552 / 19744. S2
62240 is inside the range that page calls "tens of megabytes". S4 90128 against run G's
85296 and 90672.

### The raw `BENCH` lines

```text
BENCH S1 open_ms=4357
BENCH S1 open_spawns=1102
BENCH S1 roots=100
BENCH S1 rows=4000
BENCH S1 scan_all_ms=4444
BENCH S1 scan_all_spawns=1600
BENCH S1 first_checked_ms=9086
BENCH S1 first_frame_ms=9086
BENCH S1 peak_rss_kb=19824
BENCH S1h open_ms=2273
BENCH S1h open_spawns=552
BENCH S1h roots=50
BENCH S1h rows=4000
BENCH S1h scan_all_ms=3040
BENCH S1h scan_all_spawns=800
BENCH S1h first_checked_ms=4640
BENCH S1h first_frame_ms=4640
BENCH S1h peak_rss_kb=18240
BENCH S2 file_bytes=1100001
BENCH S2 hunks=2
BENCH S2 scan_ms=248
BENCH S2 scan_spawns=16
BENCH S2 open_ms=12
BENCH S2 hunk_next_ms=15
BENCH S2 page_down_ms=10
BENCH S2 peak_rss_kb=62240
BENCH S2_default_config open_ms=14
BENCH S3 files=1000
BENCH S3 settle_ms=1559
BENCH S3 peak_rss_kb=11216
BENCH S3_events files=1000
BENCH S3_events settle_ms=1569
BENCH S3_events peak_rss_kb=10720
BENCH S4 capped_count_ms=3831
BENCH S4 settle_ms=5591
BENCH S4 peak_rss_kb=90128
BENCH S4 scan_ms=1652
BENCH S4 scan_spawns=15
BENCH S4 rows_shown=10000
BENCH S4 omitted=40000
```

## Phase 17 (S5): the worktree soak (`just bench-soak`)

Not a gate and not part of `just bench` (whose recipe skips it by name): a soak that answers
one question, how the TUI's resident set and CPU behave while several agents write into
linked worktrees of one large repository and the reader keeps accepting. The hypotheses it
is built to separate (H1 to H6) and the reasoning behind every default are in
`docs/spec/170-phase17-kickoff.md`; this section is how to run it and how to read it.

### What it does

`just bench-soak` builds the release binary and runs one `#[ignore]`d test,
`bench_s5_worktree_soak` in `crates/lastcall/tests/test_bench.rs`. A debug build prints
`SKIP: S5 measures the release build only (run: just bench-soak)` and measures nothing.

- **Fixture**, built before anything is measured, under the scenario's `TempDir`s: one
  repository `W/repo` of 300 committed text files of 3,000 lines each, 38 bytes a line:
  34.2 MB, which is 32.6 MiB, of text per checkout (`fixture_mb` prints MiB, so 32.6), plus six linked worktrees `W/w01` to `W/w06` made with `git worktree add -b
  agent/wNN`, siblings of the repository. An in-process engine takes first sight and asserts
  seven roots; the TUI must then show `nothing pending across 7 repos`, and
  `worktree_badges` counts the worktrees its summary lists with the `[worktree of repo]`
  badge (six).
- **Isolation** as every scenario: `isolated_lastcall` (private `HOME`, config and
  `LASTCALL_STATE_DIR` in the temp dir, no `HERDR_*`, update check and welcome off). The TUI
  runs as `lastcall tui` with the default timings (debounce 750 ms, 3 s cap), not `--poll 1`.
- **Writers**: one thread per worktree; the main checkout stays idle. Every 250 ms a writer
  rewrites its next file (round robin), editing line `j` when `j % stride == seq % stride`
  (stride 150 by default): 20 lines of a 3,000-line file, 150 apart, a different set and a
  new stamp each time. Under the diff's 3 lines of context those are 20 separate hunks, so
  the header's hunks run near 20 times its files and H1 (every diff resident) can be told
  from H2 (whole blobs per scan). A stride of 5 merges the edits into one whole-file hunk.
  During churn phases only, every 240 writes (about a minute) a writer
  runs `git add -A`, `git commit` and `git switch -c agent/wNN-<k>`, leaving the old branch, so
  parked records pile up behind the accepts and name distinct trees. A git failure is printed
  (`--- S5 writer …`) and the writer carries on; an `index.lock` in that message would be news.
- **Cycle**, three times: **ramp** 90 s (writers on, nothing accepted); **churn** 150 s (writers
  on, `ctrl-a` every 20 s, `y` only once the `Accept all …?` question is on screen, never
  blind, because a stray `y` is `copy`); **quiet** 60 s (writers stopped and joined, then the
  final accept, then idle).
- **Relaunch**: writers on for 60 s with nothing accepted (`phase=prequit`), writers stopped
  and joined, `q`, then the TUI started again over the same state and sampled for 30 s: the
  launch's `scan_all` is where up to eight roots' batches are in flight at once.
- **Sampling**: a harness thread runs `ps -o rss=,time= -p <pid>` once a second. CPU % is the
  delta of the cumulative CPU time over the wall delta (not `ps -o %cpu`, a decaying average on
  macOS), so it can exceed 100 on several cores. It is lastcall's own user plus system time
  and excludes its git children, which is also what Activity Monitor shows for the process.
  The 10 ms sampler the other scenarios use is off, except for the relaunch (below). At every
  phase's end, `footprint <pid>` then `heap <pid>` run (macOS only, each time-boxed to 30 s;
  `n/a` when absent or timed out), after the phase's closing sample. On a loaded phase the
  writers are still writing, so the two tools' readings are **seconds apart** and do not
  compare with each other; `heap`'s own banner (`Physical footprint:`) and its in-use total
  are taken while it holds the task suspended, so that pair is one instant. `leaks <pid>`
  runs once, on the relaunched process at the very end. The tools' output files go to their
  own temp dir, beside the state dir.
- **The tools pause the child.** `heap` and `leaks` suspend the target for about a second
  per GB it maps, so the next phase opens with a catch-up burst (the watcher's queued events
  scanned at once): the first trace line of a phase can show a CPU spike that belongs to the
  pause, not to the phase.
- **Relaunch peak**: the relaunched TUI is spawned with the harness's 10 ms RSS sampler on,
  and `relaunch_peak_rss_kb` is its maximum read at the end of the 30 s window, before any
  tool runs: the launch's `scan_all` peak lasts about a second, which the 1 s sampler can
  miss. The 1 s sampler's own maximum prints beside it as `relaunch_peak_rss_kb_1hz`.
- **Scale knobs**: `LASTCALL_BENCH_S5_SCALE` (default 1.0) multiplies every phase length and
  the 240-write branch interval (so a short rehearsal still parks records); the 20 s accept
  cadence and the 250 ms write period are the user's and do not scale.
  `LASTCALL_BENCH_S5_FILES` and `LASTCALL_BENCH_S5_LINES` resize the fixture, and
  `LASTCALL_BENCH_S5_STRIDE` (default 150) sets the edit stride; the knobs line and
  `BENCH S5 stride=` print it. At scale 0.2 the run takes about four minutes; at 1.0 about
  17 minutes plus the fixture.

### What it prints

All on stderr, `BENCH S5 <metric>=<value>`; `n/a` where a tool is missing or timed out.

| metric | what it is |
|---|---|
| `trace t=<s> cycle=<n> phase=<name> rss_kb=<n> cpu_pct=<n>` | the curve: one line every 10 s of the run (`t` from the first spawn), and one at every accept with `event=accept` (`event=refused` when a churn `ctrl-a` started nothing). `cycle=0 phase=launch` is before the first ramp; `cycle=4` is the relaunch (`prequit`, then `relaunch`) |
| `c<n>_<phase>_end_rss_kb`, `_peak_rss_kb` | the RSS sampled as the phase closed, and the largest sample in it |
| `c<n>_<phase>_cpu_pct_avg` | the child's CPU seconds over the phase divided by its wall time |
| `c<n>_<phase>_end_footprint_kb` | `footprint`'s `Footprint:` total (its unit read, KB, MB or GB) |
| `c<n>_<phase>_end_reclaimable_kb` | the `Reclaimable` column summed over the `MALLOC*` rows, from the same `footprint` run |
| `c<n>_<phase>_end_heap_footprint_kb` | `heap`'s banner `Physical footprint:` (unit glued on: `752K`, `56.2M`), at the instant `heap` suspended the task |
| `c<n>_<phase>_end_heap_inuse_kb` | `heap`'s `All zones: … (<bytes> bytes)`: what malloc holds as live allocations, the same instant as `heap_footprint` |
| `c<n>_{ramp,churn}_end_files`, `_end_hunks` | the header's counts as the phase closed |
| `c<n>_churn_accepts_started` | accepts that actually started (an accept already running refuses another) |
| `c<n>_quiet_settle_s` | seconds from the final accept to a screen with nothing pending; `SKIP` when that never happens inside the quiet phase |
| `c<n>_quiet_accepts_started` | accepts the quiet phase started (more than one means rows came back after the first) |
| `prequit_*`, `relaunch_*` | the same readings for the relaunch; `relaunch_first_frame_s` is spawn to a header with counts (the launch hold over); `relaunch_peak_rss_kb` is the 10 ms sampler's, `relaunch_peak_rss_kb_1hz` the 1 s sampler's |
| `stride` | the writers' edit stride (the knob) |
| `leaks_count`, `leaks_bytes` | `leaks --nostacks --noContent` on the relaunched process: unreachable blocks only |
| `parked_records_max` | the most parked records any one root's `ledger.json` holds after the run |
| `parked_trees_distinct` | distinct parked `seen_tree` oids, counted per root and summed (each root has its own tree cache) |
| `writer_switches`, `writer_failures` | the writers' branch switches and failed writes or git steps |
| `roots`, `worktree_badges`, `fixture_mb`, `fixture_s`, `wall_s` | the shape, one checkout's text size, the fixture's build time, the whole test's wall time |

**The quiet phase in detail.** After the writers are joined the harness waits for the header
to hold still for 5 s (at most half the phase), so the final accept covers the writers' last
writes instead of racing the watcher's scans of them. It then sends `ctrl-a` (+ `y`), and while
rows remain it sends `ctrl-a` again every 5 s; a refusal while the first accept runs costs
nothing. `settle_s` runs from the first of those accepts.

### Reading it

Absolute RSS moves 15 to 33 % between runs on this machine (see "Variance" above), so the
curve is read as **deltas within one run**:

- **quiet-end against quiet-end across the three cycles** (`c1_quiet_end_*`,
  `c2_quiet_end_*`, `c3_quiet_end_*`). Flat means memory comes back after the accepts; a
  steady climb with the same pile shape is growth to explain.
- **peak against quiet-end within a cycle**: how much of the churn peak the process gives
  back once nothing is pending.
- **`heap_footprint` against `heap_inuse`, one instant** (both from the same `heap` run):
  in-use low while the footprint stays high is the allocator keeping freed regions (H6,
  retention); both high is live data (H1 to H4). Do not pair `footprint`'s total with
  `heap_inuse` on a loaded phase: they are seconds apart while the writers write.
- **`leaks` finds unreachable blocks only.** Memory that is still reachable but keeps
  growing (a cache never trimmed, parked trees) is not a leak to it; that growth is told by
  the quiet-end trend across cycles and by `parked_records_max` / `parked_trees_distinct`
  (H5), never by `leaks_count=0`.
- **relaunch peak against the c3 churn peak**: whether launch's parallel `scan_all` is the
  worst moment of the run. The churn peaks are 1 s samples, so compare them with
  `relaunch_peak_rss_kb_1hz` like for like; `relaunch_peak_rss_kb` (10 ms) is the true
  launch peak.
- `cpu_pct` is lastcall's own CPU (its git children excluded) under a stress well above the
  reported 10 %; read its shape (does it fall in quiet, does it climb cycle to cycle), not
  its level, and discount a spike on a phase's first trace line (the tools' pause, above).
- On macOS `ps`'s RSS still counts pages malloc has freed and marked reclaimable until the
  kernel takes them back; the footprints do not, and `footprint`'s `Reclaimable` column is
  those pages. An RSS several times the footprint, with `reclaimable_kb` near the
  difference, is that accounting, not live memory (the first scale-0.2 rehearsal, at stride
  5, showed it at every phase end after the first churn).
- Below scale 0.2 the quiet phase is too short for the still-header wait plus a 5 s retry,
  so `quiet_settle_s` prints `SKIP` there by construction; read it only at 0.2 and above.

### Runs A and B

Machine: as "Machine" above (Apple M1 Pro, 10 CPUs, 32 GB; macOS 15.7.4 (24G517); git
2.37.1 (Apple Git-137.1); rustc / cargo 1.98.0; `[profile.release] strip = "debuginfo"`;
FSEvents). Date 2026-10-26, commit `bf975cb`, scale 1.0, default sizes (300 files of 3,000
lines, six worktrees). Runs A and B at the default stride (150); run C at
`LASTCALL_BENCH_S5_STRIDE=5`, the heavy whole-file shape, for the slope. The maintainer's
own lastcall was open throughout (idle, 31 MB), the only other load.

| | run A (stride 150) | run B (stride 150) | run C (stride 5) |
|---|---|---|---|
| files / hunks pending at c1 ramp end | 1800 / 36000 | 1800 / 36000 | 1800 / 1800 |
| c1 ramp end, heap in-use (MB) | 52 | 128 | 1,046 |
| c2 ramp end, heap in-use (MB) | 133 | 86 | 1,112 |
| c3 ramp end, heap in-use (MB) | 55 | 55 | 1,081 |
| c1 churn peak RSS (MB) | 735 | 586 | 2,306 |
| c2 churn peak RSS (MB) | 712 | 675 | 2,111 |
| c3 churn peak RSS (MB) | 684 | 720 | 1,978 |
| c1 quiet end, heap in-use (MB) | 2.4 | 2.5 | 2.5 |
| c2 quiet end, heap in-use (MB) | 3.7 | 3.7 | 3.7 |
| c3 quiet end, heap in-use (MB) | 5.2 | 5.2 | 5.2 |
| c1 quiet end, heap footprint (MB) | 132 | 148 | 178 |
| c2 quiet end, heap footprint (MB) | 206 | 168 | 232 |
| c3 quiet end, heap footprint (MB) | 206 | 179 | 256 |
| c1 quiet end, RSS (MB) | 506 | 426 | 209 |
| c2 quiet end, RSS (MB) | 251 | 620 | 247 |
| c3 quiet end, RSS (MB) | 423 | 342 | 207 |
| relaunch peak RSS, 10 ms (MB) | 827 | 835 | 1,508 |
| relaunch end, heap in-use (MB) | 45 | 45 | 831 |
| ramp CPU % (c1) | 26.7 | 26.7 | 32.2 |
| quiet CPU % (c3) | 2.6 | 2.8 | 3.7 |
| leaks | 0 | 0 | 0 |
| parked trees, distinct | 42 | 42 | 42 |
| wall s | 1009 | 1009 | 1038 |

Run A (`wall_s=1009`):

```
BENCH S5 stride=150
BENCH S5 fixture_mb=32.6
BENCH S5 fixture_s=2.0
BENCH S5 roots=7
BENCH S5 worktree_badges=6
BENCH S5 c1_ramp_end_rss_kb=675216
BENCH S5 c1_ramp_peak_rss_kb=675216
BENCH S5 c1_ramp_cpu_pct_avg=26.7
BENCH S5 c1_ramp_end_footprint_kb=167936
BENCH S5 c1_ramp_end_reclaimable_kb=502000
BENCH S5 c1_ramp_end_heap_footprint_kb=167834
BENCH S5 c1_ramp_end_heap_inuse_kb=53325
BENCH S5 c1_ramp_end_files=1800
BENCH S5 c1_ramp_end_hunks=36000
BENCH S5 c1_churn_end_rss_kb=685072
BENCH S5 c1_churn_peak_rss_kb=753136
BENCH S5 c1_churn_cpu_pct_avg=21.3
BENCH S5 c1_churn_end_footprint_kb=133120
BENCH S5 c1_churn_end_reclaimable_kb=546816
BENCH S5 c1_churn_end_heap_footprint_kb=132915
BENCH S5 c1_churn_end_heap_inuse_kb=12423
BENCH S5 c1_churn_end_files=347
BENCH S5 c1_churn_end_hunks=6940
BENCH S5 c1_churn_accepts_started=7
BENCH S5 c1_quiet_settle_s=2.5
BENCH S5 c1_quiet_accepts_started=1
BENCH S5 c1_quiet_end_rss_kb=518352
BENCH S5 c1_quiet_peak_rss_kb=685744
BENCH S5 c1_quiet_cpu_pct_avg=4.5
BENCH S5 c1_quiet_end_footprint_kb=135168
BENCH S5 c1_quiet_end_reclaimable_kb=378880
BENCH S5 c1_quiet_end_heap_footprint_kb=135475
BENCH S5 c1_quiet_end_heap_inuse_kb=2508
BENCH S5 c2_ramp_end_rss_kb=648064
BENCH S5 c2_ramp_peak_rss_kb=654752
BENCH S5 c2_ramp_cpu_pct_avg=27.1
BENCH S5 c2_ramp_end_footprint_kb=220160
BENCH S5 c2_ramp_end_reclaimable_kb=428256
BENCH S5 c2_ramp_end_heap_footprint_kb=219853
BENCH S5 c2_ramp_end_heap_inuse_kb=136431
BENCH S5 c2_ramp_end_files=1800
BENCH S5 c2_ramp_end_hunks=36000
BENCH S5 c2_churn_end_rss_kb=678896
BENCH S5 c2_churn_peak_rss_kb=729056
BENCH S5 c2_churn_cpu_pct_avg=20.2
BENCH S5 c2_churn_end_footprint_kb=202752
BENCH S5 c2_churn_end_reclaimable_kb=477056
BENCH S5 c2_churn_end_heap_footprint_kb=202342
BENCH S5 c2_churn_end_heap_inuse_kb=10549
BENCH S5 c2_churn_end_files=197
BENCH S5 c2_churn_end_hunks=3940
BENCH S5 c2_churn_accepts_started=7
BENCH S5 c2_quiet_settle_s=2.5
BENCH S5 c2_quiet_accepts_started=1
BENCH S5 c2_quiet_end_rss_kb=257328
BENCH S5 c2_quiet_peak_rss_kb=679568
BENCH S5 c2_quiet_cpu_pct_avg=2.9
BENCH S5 c2_quiet_end_footprint_kb=210944
BENCH S5 c2_quiet_end_reclaimable_kb=83968
BENCH S5 c2_quiet_end_heap_footprint_kb=210637
BENCH S5 c2_quiet_end_heap_inuse_kb=3796
BENCH S5 c3_ramp_end_rss_kb=655648
BENCH S5 c3_ramp_peak_rss_kb=655648
BENCH S5 c3_ramp_cpu_pct_avg=28.5
BENCH S5 c3_ramp_end_footprint_kb=238592
BENCH S5 c3_ramp_end_reclaimable_kb=440544
BENCH S5 c3_ramp_end_heap_footprint_kb=229376
BENCH S5 c3_ramp_end_heap_inuse_kb=55921
BENCH S5 c3_ramp_end_files=1800
BENCH S5 c3_ramp_end_hunks=36000
BENCH S5 c3_churn_end_rss_kb=433216
BENCH S5 c3_churn_peak_rss_kb=700128
BENCH S5 c3_churn_cpu_pct_avg=22.8
BENCH S5 c3_churn_end_footprint_kb=207872
BENCH S5 c3_churn_end_reclaimable_kb=262144
BENCH S5 c3_churn_end_heap_footprint_kb=207974
BENCH S5 c3_churn_end_heap_inuse_kb=8752
BENCH S5 c3_churn_end_files=118
BENCH S5 c3_churn_end_hunks=2360
BENCH S5 c3_churn_accepts_started=7
BENCH S5 c3_quiet_settle_s=1.7
BENCH S5 c3_quiet_accepts_started=1
BENCH S5 c3_quiet_end_rss_kb=432944
BENCH S5 c3_quiet_peak_rss_kb=437856
BENCH S5 c3_quiet_cpu_pct_avg=2.6
BENCH S5 c3_quiet_end_footprint_kb=210944
BENCH S5 c3_quiet_end_reclaimable_kb=259072
BENCH S5 c3_quiet_end_heap_footprint_kb=211046
BENCH S5 c3_quiet_end_heap_inuse_kb=5327
BENCH S5 prequit_end_rss_kb=523648
BENCH S5 prequit_peak_rss_kb=523648
BENCH S5 prequit_end_files=1303
BENCH S5 prequit_end_hunks=26060
BENCH S5 relaunch_first_frame_s=1.4
BENCH S5 relaunch_peak_rss_kb=846528
BENCH S5 relaunch_peak_rss_kb_1hz=846528
BENCH S5 relaunch_end_rss_kb=846528
BENCH S5 relaunch_cpu_pct_avg=8.6
BENCH S5 relaunch_end_footprint_kb=397312
BENCH S5 relaunch_end_reclaimable_kb=444432
BENCH S5 relaunch_end_heap_footprint_kb=397005
BENCH S5 relaunch_end_heap_inuse_kb=46162
BENCH S5 leaks_count=0
BENCH S5 leaks_bytes=0
BENCH S5 parked_records_max=7
BENCH S5 parked_trees_distinct=42
BENCH S5 writer_switches=42
BENCH S5 writer_failures=0
BENCH S5 wall_s=1009
```

Run B (`wall_s=1009`):

```
BENCH S5 stride=150
BENCH S5 fixture_mb=32.6
BENCH S5 fixture_s=2.0
BENCH S5 roots=7
BENCH S5 worktree_badges=6
BENCH S5 c1_ramp_end_rss_kb=628704
BENCH S5 c1_ramp_peak_rss_kb=686384
BENCH S5 c1_ramp_cpu_pct_avg=26.7
BENCH S5 c1_ramp_end_footprint_kb=185344
BENCH S5 c1_ramp_end_reclaimable_kb=440560
BENCH S5 c1_ramp_end_heap_footprint_kb=199987
BENCH S5 c1_ramp_end_heap_inuse_kb=131584
BENCH S5 c1_ramp_end_files=1800
BENCH S5 c1_ramp_end_hunks=36000
BENCH S5 c1_churn_end_rss_kb=468000
BENCH S5 c1_churn_peak_rss_kb=600336
BENCH S5 c1_churn_cpu_pct_avg=21.2
BENCH S5 c1_churn_end_footprint_kb=147456
BENCH S5 c1_churn_end_reclaimable_kb=317440
BENCH S5 c1_churn_end_heap_footprint_kb=147968
BENCH S5 c1_churn_end_heap_inuse_kb=7573
BENCH S5 c1_churn_end_files=178
BENCH S5 c1_churn_end_hunks=3560
BENCH S5 c1_churn_accepts_started=7
BENCH S5 c1_quiet_settle_s=2.4
BENCH S5 c1_quiet_accepts_started=1
BENCH S5 c1_quiet_end_rss_kb=436112
BENCH S5 c1_quiet_peak_rss_kb=457904
BENCH S5 c1_quiet_cpu_pct_avg=3.3
BENCH S5 c1_quiet_end_footprint_kb=151552
BENCH S5 c1_quiet_end_reclaimable_kb=280576
BENCH S5 c1_quiet_end_heap_footprint_kb=151962
BENCH S5 c1_quiet_end_heap_inuse_kb=2516
BENCH S5 c2_ramp_end_rss_kb=627504
BENCH S5 c2_ramp_peak_rss_kb=638432
BENCH S5 c2_ramp_cpu_pct_avg=27.6
BENCH S5 c2_ramp_end_footprint_kb=183296
BENCH S5 c2_ramp_end_reclaimable_kb=441568
BENCH S5 c2_ramp_end_heap_footprint_kb=183296
BENCH S5 c2_ramp_end_heap_inuse_kb=87853
BENCH S5 c2_ramp_end_files=1800
BENCH S5 c2_ramp_end_hunks=36000
BENCH S5 c2_churn_end_rss_kb=612224
BENCH S5 c2_churn_peak_rss_kb=691648
BENCH S5 c2_churn_cpu_pct_avg=19.9
BENCH S5 c2_churn_end_footprint_kb=167936
BENCH S5 c2_churn_end_reclaimable_kb=440688
BENCH S5 c2_churn_end_heap_footprint_kb=168141
BENCH S5 c2_churn_end_heap_inuse_kb=11594
BENCH S5 c2_churn_end_files=273
BENCH S5 c2_churn_end_hunks=5460
BENCH S5 c2_churn_accepts_started=7
BENCH S5 c2_quiet_settle_s=1.6
BENCH S5 c2_quiet_accepts_started=1
BENCH S5 c2_quiet_end_rss_kb=634448
BENCH S5 c2_quiet_peak_rss_kb=634448
BENCH S5 c2_quiet_cpu_pct_avg=3.0
BENCH S5 c2_quiet_end_footprint_kb=172032
BENCH S5 c2_quiet_end_reclaimable_kb=459120
BENCH S5 c2_quiet_end_heap_footprint_kb=172134
BENCH S5 c2_quiet_end_heap_inuse_kb=3795
BENCH S5 c3_ramp_end_rss_kb=658032
BENCH S5 c3_ramp_peak_rss_kb=689072
BENCH S5 c3_ramp_cpu_pct_avg=28.3
BENCH S5 c3_ramp_end_footprint_kb=177152
BENCH S5 c3_ramp_end_reclaimable_kb=478432
BENCH S5 c3_ramp_end_heap_footprint_kb=176742
BENCH S5 c3_ramp_end_heap_inuse_kb=55920
BENCH S5 c3_ramp_end_files=1800
BENCH S5 c3_ramp_end_hunks=36000
BENCH S5 c3_churn_end_rss_kb=353472
BENCH S5 c3_churn_peak_rss_kb=737536
BENCH S5 c3_churn_cpu_pct_avg=21.7
BENCH S5 c3_churn_end_footprint_kb=181248
BENCH S5 c3_churn_end_reclaimable_kb=178832
BENCH S5 c3_churn_end_heap_footprint_kb=181555
BENCH S5 c3_churn_end_heap_inuse_kb=9678
BENCH S5 c3_churn_end_files=153
BENCH S5 c3_churn_end_hunks=3060
BENCH S5 c3_churn_accepts_started=7
BENCH S5 c3_quiet_settle_s=1.8
BENCH S5 c3_quiet_accepts_started=1
BENCH S5 c3_quiet_end_rss_kb=350000
BENCH S5 c3_quiet_peak_rss_kb=359216
BENCH S5 c3_quiet_cpu_pct_avg=2.8
BENCH S5 c3_quiet_end_footprint_kb=183296
BENCH S5 c3_quiet_end_reclaimable_kb=175760
BENCH S5 c3_quiet_end_heap_footprint_kb=183706
BENCH S5 c3_quiet_end_heap_inuse_kb=5320
BENCH S5 prequit_end_rss_kb=525552
BENCH S5 prequit_peak_rss_kb=525552
BENCH S5 prequit_end_files=1332
BENCH S5 prequit_end_hunks=26640
BENCH S5 relaunch_first_frame_s=1.4
BENCH S5 relaunch_peak_rss_kb=855184
BENCH S5 relaunch_peak_rss_kb_1hz=855184
BENCH S5 relaunch_end_rss_kb=855184
BENCH S5 relaunch_cpu_pct_avg=8.5
BENCH S5 relaunch_end_footprint_kb=407552
BENCH S5 relaunch_end_reclaimable_kb=443424
BENCH S5 relaunch_end_heap_footprint_kb=407347
BENCH S5 relaunch_end_heap_inuse_kb=46177
BENCH S5 leaks_count=0
BENCH S5 leaks_bytes=0
BENCH S5 parked_records_max=7
BENCH S5 parked_trees_distinct=42
BENCH S5 writer_switches=42
BENCH S5 writer_failures=0
BENCH S5 wall_s=1009
```

Run C, stride 5 (`wall_s=1038`):

```
BENCH S5 stride=5
BENCH S5 fixture_mb=32.6
BENCH S5 fixture_s=2.4
BENCH S5 roots=7
BENCH S5 worktree_badges=6
BENCH S5 c1_ramp_end_rss_kb=2024256
BENCH S5 c1_ramp_peak_rss_kb=2024896
BENCH S5 c1_ramp_cpu_pct_avg=32.2
BENCH S5 c1_ramp_end_footprint_kb=814080
BENCH S5 c1_ramp_end_reclaimable_kb=1192352
BENCH S5 c1_ramp_end_heap_footprint_kb=798515
BENCH S5 c1_ramp_end_heap_inuse_kb=1071465
BENCH S5 c1_ramp_end_files=1800
BENCH S5 c1_ramp_end_hunks=1800
BENCH S5 c1_churn_end_rss_kb=711696
BENCH S5 c1_churn_peak_rss_kb=2361712
BENCH S5 c1_churn_cpu_pct_avg=24.0
BENCH S5 c1_churn_end_footprint_kb=194560
BENCH S5 c1_churn_end_reclaimable_kb=524400
BENCH S5 c1_churn_end_heap_footprint_kb=196813
BENCH S5 c1_churn_end_heap_inuse_kb=179583
BENCH S5 c1_churn_end_files=298
BENCH S5 c1_churn_end_hunks=298
BENCH S5 c1_churn_accepts_started=7
BENCH S5 c1_quiet_settle_s=2.5
BENCH S5 c1_quiet_accepts_started=1
BENCH S5 c1_quiet_end_rss_kb=213760
BENCH S5 c1_quiet_peak_rss_kb=739008
BENCH S5 c1_quiet_cpu_pct_avg=3.9
BENCH S5 c1_quiet_end_footprint_kb=182272
BENCH S5 c1_quiet_end_reclaimable_kb=157696
BENCH S5 c1_quiet_end_heap_footprint_kb=182682
BENCH S5 c1_quiet_end_heap_inuse_kb=2523
BENCH S5 c2_ramp_end_rss_kb=1741344
BENCH S5 c2_ramp_peak_rss_kb=1741344
BENCH S5 c2_ramp_cpu_pct_avg=31.1
BENCH S5 c2_ramp_end_footprint_kb=1042432
BENCH S5 c2_ramp_end_reclaimable_kb=776192
BENCH S5 c2_ramp_end_heap_footprint_kb=1010483
BENCH S5 c2_ramp_end_heap_inuse_kb=1138710
BENCH S5 c2_ramp_end_files=1800
BENCH S5 c2_ramp_end_hunks=1800
BENCH S5 c2_churn_end_rss_kb=567024
BENCH S5 c2_churn_peak_rss_kb=2161264
BENCH S5 c2_churn_cpu_pct_avg=23.2
BENCH S5 c2_churn_end_footprint_kb=236544
BENCH S5 c2_churn_end_reclaimable_kb=435424
BENCH S5 c2_churn_end_heap_footprint_kb=236954
BENCH S5 c2_churn_end_heap_inuse_kb=119681
BENCH S5 c2_churn_end_files=195
BENCH S5 c2_churn_end_hunks=195
BENCH S5 c2_churn_accepts_started=7
BENCH S5 c2_quiet_settle_s=2.5
BENCH S5 c2_quiet_accepts_started=1
BENCH S5 c2_quiet_end_rss_kb=253200
BENCH S5 c2_quiet_peak_rss_kb=585232
BENCH S5 c2_quiet_cpu_pct_avg=3.4
BENCH S5 c2_quiet_end_footprint_kb=237568
BENCH S5 c2_quiet_end_reclaimable_kb=178176
BENCH S5 c2_quiet_end_heap_footprint_kb=237568
BENCH S5 c2_quiet_end_heap_inuse_kb=3818
BENCH S5 c3_ramp_end_rss_kb=1742240
BENCH S5 c3_ramp_peak_rss_kb=1761664
BENCH S5 c3_ramp_cpu_pct_avg=32.1
BENCH S5 c3_ramp_end_footprint_kb=1115136
BENCH S5 c3_ramp_end_reclaimable_kb=791552
BENCH S5 c3_ramp_end_heap_footprint_kb=1048576
BENCH S5 c3_ramp_end_heap_inuse_kb=1107353
BENCH S5 c3_ramp_end_files=1800
BENCH S5 c3_ramp_end_hunks=1800
BENCH S5 c3_churn_end_rss_kb=685856
BENCH S5 c3_churn_peak_rss_kb=2025136
BENCH S5 c3_churn_cpu_pct_avg=24.2
BENCH S5 c3_churn_end_footprint_kb=266240
BENCH S5 c3_churn_end_reclaimable_kb=490784
BENCH S5 c3_churn_end_heap_footprint_kb=265830
BENCH S5 c3_churn_end_heap_inuse_kb=120017
BENCH S5 c3_churn_end_files=193
BENCH S5 c3_churn_end_hunks=193
BENCH S5 c3_churn_accepts_started=7
BENCH S5 c3_quiet_settle_s=2.6
BENCH S5 c3_quiet_accepts_started=1
BENCH S5 c3_quiet_end_rss_kb=211856
BENCH S5 c3_quiet_peak_rss_kb=716656
BENCH S5 c3_quiet_cpu_pct_avg=3.7
BENCH S5 c3_quiet_end_footprint_kb=261120
BENCH S5 c3_quiet_end_reclaimable_kb=133120
BENCH S5 c3_quiet_end_heap_footprint_kb=261632
BENCH S5 c3_quiet_end_heap_inuse_kb=5348
BENCH S5 prequit_end_rss_kb=1162240
BENCH S5 prequit_peak_rss_kb=1310608
BENCH S5 prequit_end_files=1323
BENCH S5 prequit_end_hunks=1323
BENCH S5 relaunch_first_frame_s=1.8
BENCH S5 relaunch_peak_rss_kb=1543680
BENCH S5 relaunch_peak_rss_kb_1hz=1543040
BENCH S5 relaunch_end_rss_kb=780688
BENCH S5 relaunch_cpu_pct_avg=10.4
BENCH S5 relaunch_end_footprint_kb=1218560
BENCH S5 relaunch_end_reclaimable_kb=2000
BENCH S5 relaunch_end_heap_footprint_kb=1258291
BENCH S5 relaunch_end_heap_inuse_kb=850485
BENCH S5 leaks_count=0
BENCH S5 leaks_bytes=0
BENCH S5 parked_records_max=7
BENCH S5 parked_trees_distinct=42
BENCH S5 writer_switches=42
BENCH S5 writer_failures=0
BENCH S5 wall_s=1038
```

The `trace` lines (the per-10 s curve) are left out here; rerun `just bench-soak` to see
them. The reading of these runs is the Part B addendum of
`docs/spec/170-phase17-kickoff.md`.

### Phase 17 Part B (runs D and E): the four fixes

Machine block as runs A to C; date 2026-10-26; commit `803b33d` (fixes 1 to 4 of the
kickoff's Part B plus the code review's two folds). Run D at the default stride (150), run
E at stride 5, both at scale 1.0, one after the other on a machine whose only other load
was the maintainer's idle lastcall. Read against runs A and B (stride 150) and run C
(stride 5) above.

| | A / B (before) | **D (after)** | C (before) | **E (after)** |
|---|---|---|---|---|
| stride | 150 | 150 | 5 | 5 |
| c1 / c2 / c3 ramp end, heap in-use (MB) | 52-128 / 86-133 / 55 | **18 / 20 / 21** | 1,046 / 1,112 / 1,081 | **334 / 279 / 280** |
| c1 / c2 / c3 churn peak RSS (MB) | 586-735 / 675-712 / 684-720 | **118 / 129 / 161** | 2,306 / 2,111 / 1,978 | **408 / 351 / 385** |
| c1 / c2 / c3 quiet end, heap footprint (MB) | 132-148 / 168-206 / 179-206 | **43 / 45 / 63** | 178 / 232 / 256 | **65 / 83 / 47** |
| c1 / c2 / c3 quiet end, heap in-use (MB) | 2.4 / 3.7 / 5.2 | 2.5 / 3.7 / 5.2 | 2.5 / 3.7 / 5.2 | 2.5 / 3.7 / 5.2 |
| relaunch peak RSS, 10 ms (MB) | 827 / 835 | **767** | 1,508 | **986** |
| relaunch end, heap footprint (MB) | 388 / 398 | **310** | 1,229 | **581** |
| relaunch end, heap in-use (MB) | 45 | **18** | 831 | **221** |
| ramp CPU % (c1) | 26.7 | **4.8** | 32.2 | **4.4** |
| quiet CPU % (c3) | 2.6 / 2.8 | **1.2** | 3.7 | **1.2** |
| leaks | 0 | 0 | 0 | 0 |
| wall s | 1009 | 1006 | 1038 | 1008 |

**Per fix**, stride 5 at `LASTCALL_BENCH_S5_SCALE=0.34` (about 650 files pending at each
ramp's end), one run per commit, so each fix's share shows:

| commit | c1 ramp end, heap in-use (MB) | c1 churn peak RSS (MB) | c1 / c2 / c3 quiet end, footprint (MB) | c1 ramp CPU % |
|---|---|---|---|---|
| `1b074c3`, before | 423 | 1,899 | 340 / 731 / 1,113 | 16.0 |
| `6257d94`, fix 1: shared rows | 226 | 1,107 | 205 / 198 / 140 | 14.8 |
| `5e62b29`, fix 2: one buffer per hunk | 155 | 838 | 193 / 175 / 204 | 12.9 |
| `cf221a3`, fix 3: reuse a row's diff | 130 | 314 | 43 / 48 / 47 | **4.4** |
| `019a35b`, fix 4: blobs in chunks | 132 | 321 | 51 / 50 / 43 | 4.4 |

Reading them:
- The quiet-end footprint (Activity Monitor's Memory column) no longer climbs from cycle to
  cycle; before the fixes it doubled every cycle at stride 5 (340, 731, 1,113 MB).
- Fix 3 is most of the CPU (16 to 4.4 %) and most of the churn peak: a row whose sides did
  not move is neither fetched nor diffed again, so a scan of six busy worktrees reads only
  what changed.
- Fix 4 shows nothing at this fixture's size: one root's pending blobs (about 200 files of
  113 KB, two sides) stay under the 64 MiB chunk, so every root is still one chunk. It caps
  a larger root's batch; S5 does not build one. (The next section lowers the chunk to 8 MiB, which engages it.)
- **The relaunch is still the high point** (767 and 986 MB RSS). Up to eight roots render
  their whole piles at once, with the diff's working memory on top of the blobs, and macOS
  keeps that high-water mark: after the relaunch, live data is 18 MB (stride 150) but the
  footprint stays at 310 MB. Lowering it needs fewer roots at once at launch, a smaller
  chunk, or diffs on demand; each is a separate ruling.

`just bench`, the same commit, once (run I), against run H (Phase 11): no scenario moved
outside its noise, and S2's and S4's peaks fell (`peak_rss_kb` 62,240 to 28,512 and 90,128 to 78,336).

```
BENCH S1 open_ms=3936
BENCH S1 open_spawns=1102
BENCH S1 roots=100
BENCH S1 rows=4000
BENCH S1 scan_all_ms=4535
BENCH S1 scan_all_spawns=1600
BENCH S1 first_checked_ms=8264
BENCH S1 first_frame_ms=8264
BENCH S1 peak_rss_kb=19152
BENCH S1h open_ms=1962
BENCH S1h open_spawns=552
BENCH S1h roots=50
BENCH S1h rows=4000
BENCH S1h scan_all_ms=2818
BENCH S1h scan_all_spawns=800
BENCH S1h first_checked_ms=4207
BENCH S1h first_frame_ms=4207
BENCH S1h peak_rss_kb=17312
BENCH S2 file_bytes=1100001
BENCH S2 hunks=2
BENCH S2 scan_ms=226
BENCH S2 scan_spawns=16
BENCH S2 open_ms=15
BENCH S2 hunk_next_ms=11
BENCH S2 page_down_ms=15
BENCH S2 peak_rss_kb=28512
BENCH S2_default_config open_ms=10
BENCH S3 files=1000
BENCH S3 settle_ms=1526
BENCH S3 peak_rss_kb=11264
BENCH S3_events files=1000
BENCH S3_events settle_ms=1523
BENCH S3_events peak_rss_kb=10640
BENCH S4 capped_count_ms=3857
BENCH S4 settle_ms=4760
BENCH S4 peak_rss_kb=78336
BENCH S4 scan_ms=1595
BENCH S4 scan_spawns=15
BENCH S4 rows_shown=10000
BENCH S4 omitted=40000
```

Run D, stride 150 (`wall_s=1006`):

```
BENCH S5 stride=150
BENCH S5 fixture_mb=32.6
BENCH S5 fixture_s=2.2
BENCH S5 roots=7
BENCH S5 worktree_badges=6
BENCH S5 c1_ramp_end_rss_kb=93296
BENCH S5 c1_ramp_peak_rss_kb=93296
BENCH S5 c1_ramp_cpu_pct_avg=4.8
BENCH S5 c1_ramp_end_footprint_kb=32768
BENCH S5 c1_ramp_end_reclaimable_kb=56368
BENCH S5 c1_ramp_end_heap_footprint_kb=32666
BENCH S5 c1_ramp_end_heap_inuse_kb=18903
BENCH S5 c1_ramp_end_files=1800
BENCH S5 c1_ramp_end_hunks=36000
BENCH S5 c1_churn_end_rss_kb=100928
BENCH S5 c1_churn_peak_rss_kb=120944
BENCH S5 c1_churn_cpu_pct_avg=6.5
BENCH S5 c1_churn_end_footprint_kb=43008
BENCH S5 c1_churn_end_reclaimable_kb=55552
BENCH S5 c1_churn_end_heap_footprint_kb=42803
BENCH S5 c1_churn_end_heap_inuse_kb=6129
BENCH S5 c1_churn_end_files=379
BENCH S5 c1_churn_end_hunks=7580
BENCH S5 c1_churn_accepts_started=7
BENCH S5 c1_quiet_settle_s=1.9
BENCH S5 c1_quiet_accepts_started=1
BENCH S5 c1_quiet_end_rss_kb=88192
BENCH S5 c1_quiet_peak_rss_kb=104016
BENCH S5 c1_quiet_cpu_pct_avg=1.2
BENCH S5 c1_quiet_end_footprint_kb=44032
BENCH S5 c1_quiet_end_reclaimable_kb=42112
BENCH S5 c1_quiet_end_heap_footprint_kb=43930
BENCH S5 c1_quiet_end_heap_inuse_kb=2512
BENCH S5 c2_ramp_end_rss_kb=106384
BENCH S5 c2_ramp_peak_rss_kb=106384
BENCH S5 c2_ramp_cpu_pct_avg=4.8
BENCH S5 c2_ramp_end_footprint_kb=50176
BENCH S5 c2_ramp_end_reclaimable_kb=56448
BENCH S5 c2_ramp_end_heap_footprint_kb=50278
BENCH S5 c2_ramp_end_heap_inuse_kb=20395
BENCH S5 c2_ramp_end_files=1800
BENCH S5 c2_ramp_end_hunks=36000
BENCH S5 c2_churn_end_rss_kb=112960
BENCH S5 c2_churn_peak_rss_kb=132352
BENCH S5 c2_churn_cpu_pct_avg=6.6
BENCH S5 c2_churn_end_footprint_kb=45056
BENCH S5 c2_churn_end_reclaimable_kb=67040
BENCH S5 c2_churn_end_heap_footprint_kb=45363
BENCH S5 c2_churn_end_heap_inuse_kb=5962
BENCH S5 c2_churn_end_files=226
BENCH S5 c2_churn_end_hunks=4520
BENCH S5 c2_churn_accepts_started=7
BENCH S5 c2_quiet_settle_s=1.6
BENCH S5 c2_quiet_accepts_started=1
BENCH S5 c2_quiet_end_rss_kb=109184
BENCH S5 c2_quiet_peak_rss_kb=113024
BENCH S5 c2_quiet_cpu_pct_avg=1.1
BENCH S5 c2_quiet_end_footprint_kb=46080
BENCH S5 c2_quiet_end_reclaimable_kb=63376
BENCH S5 c2_quiet_end_heap_footprint_kb=45670
BENCH S5 c2_quiet_end_heap_inuse_kb=3793
BENCH S5 c3_ramp_end_rss_kb=89776
BENCH S5 c3_ramp_peak_rss_kb=115408
BENCH S5 c3_ramp_cpu_pct_avg=4.5
BENCH S5 c3_ramp_end_footprint_kb=53248
BENCH S5 c3_ramp_end_reclaimable_kb=37728
BENCH S5 c3_ramp_end_heap_footprint_kb=53658
BENCH S5 c3_ramp_end_heap_inuse_kb=21484
BENCH S5 c3_ramp_end_files=1800
BENCH S5 c3_ramp_end_hunks=36000
BENCH S5 c3_churn_end_rss_kb=159104
BENCH S5 c3_churn_peak_rss_kb=165104
BENCH S5 c3_churn_cpu_pct_avg=7.6
BENCH S5 c3_churn_end_footprint_kb=64512
BENCH S5 c3_churn_end_reclaimable_kb=96960
BENCH S5 c3_churn_end_heap_footprint_kb=64102
BENCH S5 c3_churn_end_heap_inuse_kb=7730
BENCH S5 c3_churn_end_files=254
BENCH S5 c3_churn_end_hunks=5080
BENCH S5 c3_churn_accepts_started=7
BENCH S5 c3_quiet_settle_s=2.4
BENCH S5 c3_quiet_accepts_started=1
BENCH S5 c3_quiet_end_rss_kb=125232
BENCH S5 c3_quiet_peak_rss_kb=160640
BENCH S5 c3_quiet_cpu_pct_avg=1.2
BENCH S5 c3_quiet_end_footprint_kb=64512
BENCH S5 c3_quiet_end_reclaimable_kb=62768
BENCH S5 c3_quiet_end_heap_footprint_kb=64307
BENCH S5 c3_quiet_end_heap_inuse_kb=5307
BENCH S5 prequit_end_rss_kb=111360
BENCH S5 prequit_peak_rss_kb=127328
BENCH S5 prequit_end_files=1365
BENCH S5 prequit_end_hunks=27300
BENCH S5 relaunch_first_frame_s=1.4
BENCH S5 relaunch_peak_rss_kb=785248
BENCH S5 relaunch_peak_rss_kb_1hz=785248
BENCH S5 relaunch_end_rss_kb=783824
BENCH S5 relaunch_cpu_pct_avg=5.6
BENCH S5 relaunch_end_footprint_kb=317440
BENCH S5 relaunch_end_reclaimable_kb=462848
BENCH S5 relaunch_end_heap_footprint_kb=317747
BENCH S5 relaunch_end_heap_inuse_kb=18918
BENCH S5 leaks_count=0
BENCH S5 leaks_bytes=0
BENCH S5 parked_records_max=7
BENCH S5 parked_trees_distinct=42
BENCH S5 writer_switches=42
BENCH S5 writer_failures=0
BENCH S5 wall_s=1006
```

Run E, stride 5 (`wall_s=1008`):

```
BENCH S5 stride=5
BENCH S5 fixture_mb=32.6
BENCH S5 fixture_s=1.9
BENCH S5 roots=7
BENCH S5 worktree_badges=6
BENCH S5 c1_ramp_end_rss_kb=358432
BENCH S5 c1_ramp_peak_rss_kb=386464
BENCH S5 c1_ramp_cpu_pct_avg=4.4
BENCH S5 c1_ramp_end_footprint_kb=329728
BENCH S5 c1_ramp_end_reclaimable_kb=80208
BENCH S5 c1_ramp_end_heap_footprint_kb=329421
BENCH S5 c1_ramp_end_heap_inuse_kb=341851
BENCH S5 c1_ramp_end_files=1800
BENCH S5 c1_ramp_end_hunks=1800
BENCH S5 c1_churn_end_rss_kb=203520
BENCH S5 c1_churn_peak_rss_kb=417440
BENCH S5 c1_churn_cpu_pct_avg=6.3
BENCH S5 c1_churn_end_footprint_kb=61440
BENCH S5 c1_churn_end_reclaimable_kb=145424
BENCH S5 c1_churn_end_heap_footprint_kb=61645
BENCH S5 c1_churn_end_heap_inuse_kb=61737
BENCH S5 c1_churn_end_files=362
BENCH S5 c1_churn_end_hunks=362
BENCH S5 c1_churn_accepts_started=7
BENCH S5 c1_quiet_settle_s=2.3
BENCH S5 c1_quiet_accepts_started=1
BENCH S5 c1_quiet_end_rss_kb=164224
BENCH S5 c1_quiet_peak_rss_kb=203728
BENCH S5 c1_quiet_cpu_pct_avg=1.1
BENCH S5 c1_quiet_end_footprint_kb=66560
BENCH S5 c1_quiet_end_reclaimable_kb=103328
BENCH S5 c1_quiet_end_heap_footprint_kb=66560
BENCH S5 c1_quiet_end_heap_inuse_kb=2512
BENCH S5 c2_ramp_end_rss_kb=319040
BENCH S5 c2_ramp_peak_rss_kb=336352
BENCH S5 c2_ramp_cpu_pct_avg=4.8
BENCH S5 c2_ramp_end_footprint_kb=262144
BENCH S5 c2_ramp_end_reclaimable_kb=105488
BENCH S5 c2_ramp_end_heap_footprint_kb=266752
BENCH S5 c2_ramp_end_heap_inuse_kb=285540
BENCH S5 c2_ramp_end_files=1800
BENCH S5 c2_ramp_end_hunks=1800
BENCH S5 c2_churn_end_rss_kb=238208
BENCH S5 c2_churn_peak_rss_kb=359840
BENCH S5 c2_churn_cpu_pct_avg=7.2
BENCH S5 c2_churn_end_footprint_kb=88064
BENCH S5 c2_churn_end_reclaimable_kb=155712
BENCH S5 c2_churn_end_heap_footprint_kb=88474
BENCH S5 c2_churn_end_heap_inuse_kb=64838
BENCH S5 c2_churn_end_files=389
BENCH S5 c2_churn_end_hunks=389
BENCH S5 c2_churn_accepts_started=7
BENCH S5 c2_quiet_settle_s=1.7
BENCH S5 c2_quiet_accepts_started=1
BENCH S5 c2_quiet_end_rss_kb=224368
BENCH S5 c2_quiet_peak_rss_kb=241824
BENCH S5 c2_quiet_cpu_pct_avg=1.3
BENCH S5 c2_quiet_end_footprint_kb=84992
BENCH S5 c2_quiet_end_reclaimable_kb=146064
BENCH S5 c2_quiet_end_heap_footprint_kb=84890
BENCH S5 c2_quiet_end_heap_inuse_kb=3795
BENCH S5 c3_ramp_end_rss_kb=345360
BENCH S5 c3_ramp_peak_rss_kb=345360
BENCH S5 c3_ramp_cpu_pct_avg=4.8
BENCH S5 c3_ramp_end_footprint_kb=279552
BENCH S5 c3_ramp_end_reclaimable_kb=124992
BENCH S5 c3_ramp_end_heap_footprint_kb=279962
BENCH S5 c3_ramp_end_heap_inuse_kb=286823
BENCH S5 c3_ramp_end_files=1800
BENCH S5 c3_ramp_end_hunks=1800
BENCH S5 c3_churn_end_rss_kb=256912
BENCH S5 c3_churn_peak_rss_kb=394032
BENCH S5 c3_churn_cpu_pct_avg=6.9
BENCH S5 c3_churn_end_footprint_kb=36864
BENCH S5 c3_churn_end_reclaimable_kb=223872
BENCH S5 c3_churn_end_heap_footprint_kb=36762
BENCH S5 c3_churn_end_heap_inuse_kb=45323
BENCH S5 c3_churn_end_files=237
BENCH S5 c3_churn_end_hunks=237
BENCH S5 c3_churn_accepts_started=7
BENCH S5 c3_quiet_settle_s=2.5
BENCH S5 c3_quiet_accepts_started=1
BENCH S5 c3_quiet_end_rss_kb=110192
BENCH S5 c3_quiet_peak_rss_kb=259648
BENCH S5 c3_quiet_cpu_pct_avg=1.2
BENCH S5 c3_quiet_end_footprint_kb=48128
BENCH S5 c3_quiet_end_reclaimable_kb=65680
BENCH S5 c3_quiet_end_heap_footprint_kb=48128
BENCH S5 c3_quiet_end_heap_inuse_kb=5301
BENCH S5 prequit_end_rss_kb=269504
BENCH S5 prequit_peak_rss_kb=269728
BENCH S5 prequit_end_files=1327
BENCH S5 prequit_end_hunks=1327
BENCH S5 relaunch_first_frame_s=1.4
BENCH S5 relaunch_peak_rss_kb=1009600
BENCH S5 relaunch_peak_rss_kb_1hz=1009600
BENCH S5 relaunch_end_rss_kb=1009600
BENCH S5 relaunch_cpu_pct_avg=6.3
BENCH S5 relaunch_end_footprint_kb=594944
BENCH S5 relaunch_end_reclaimable_kb=410624
BENCH S5 relaunch_end_heap_footprint_kb=595046
BENCH S5 relaunch_end_heap_inuse_kb=226779
BENCH S5 leaks_count=0
BENCH S5 leaks_bytes=0
BENCH S5 parked_records_max=7
BENCH S5 parked_trees_distinct=42
BENCH S5 writer_switches=42
BENCH S5 writer_failures=0
BENCH S5 wall_s=1008
```

### Phase 17 Part B (runs J and K): the 8 MiB chunk

The sponsor's ruling after runs D and E: try a smaller chunk, and keep it only if the
relaunch peak drops. Same machine; date 2026-10-30; commit `3529ae7` with only
`BLOB_CHUNK_BYTES` changed from 64 MiB to 8 MiB. Run J at stride 150, run K at stride 5.
Both engage the chunk: one root's pending blobs (about 45 MB) now take about six chunks.

| | D, 64 MiB | **J, 8 MiB** | E, 64 MiB | **K, 8 MiB** |
|---|---|---|---|---|
| stride | 150 | 150 | 5 | 5 |
| relaunch peak RSS, 10 ms (MB) | 767 | **213** | 986 | **376** |
| relaunch end, heap footprint (MB) | 310 | **31** | 581 | **89** |
| relaunch end, heap in-use (MB) | 18 | 18 | 221 | 221 |
| relaunch first frame (s) | 1.4 | 1.5 | 1.4 | 1.5 |
| c1 / c2 / c3 churn peak RSS (MB) | 118 / 129 / 161 | 98 / 104 / 124 | 408 / 351 / 385 | 358 / 355 / 365 |
| c1 / c2 / c3 quiet end, heap footprint (MB) | 43 / 45 / 63 | 24 / 30 / 37 | 65 / 83 / 47 | 62 / 72 / 78 |
| c1 ramp CPU % | 4.8 | 4.6 | 4.4 | 4.8 |
| leaks | 0 | 0 | 0 | 0 |

The relaunch is no longer the run's high point by much: at stride 5 its peak (376 MB) sits
beside the churn peaks. Live data is unchanged, as it should be: the chunk only changes how
many blobs are held at once. `just bench` with the same change (run L) against run I: every
spawn count is identical, and the wall times are within the noise this page records.

```
BENCH S1 open_ms=4839
BENCH S1 open_spawns=1102
BENCH S1 roots=100
BENCH S1 rows=4000
BENCH S1 scan_all_ms=4541
BENCH S1 scan_all_spawns=1600
BENCH S1 first_checked_ms=9566
BENCH S1 first_frame_ms=9567
BENCH S1 peak_rss_kb=19248
BENCH S1h open_ms=2311
BENCH S1h open_spawns=552
BENCH S1h roots=50
BENCH S1h rows=4000
BENCH S1h scan_all_ms=3289
BENCH S1h scan_all_spawns=800
BENCH S1h first_checked_ms=4727
BENCH S1h first_frame_ms=4728
BENCH S1h peak_rss_kb=17488
BENCH S2 file_bytes=1100001
BENCH S2 hunks=2
BENCH S2 scan_ms=205
BENCH S2 scan_spawns=16
BENCH S2 open_ms=15
BENCH S2 hunk_next_ms=16
BENCH S2 page_down_ms=11
BENCH S2 peak_rss_kb=30576
BENCH S2_default_config open_ms=10
BENCH S3 files=1000
BENCH S3 settle_ms=1587
BENCH S3 peak_rss_kb=10848
BENCH S3_events files=1000
BENCH S3_events settle_ms=1587
BENCH S3_events peak_rss_kb=10944
BENCH S4 capped_count_ms=4409
BENCH S4 settle_ms=5381
BENCH S4 peak_rss_kb=92688
BENCH S4 scan_ms=1739
BENCH S4 scan_spawns=15
BENCH S4 rows_shown=10000
BENCH S4 omitted=40000
```

Run J, stride 150 (`wall_s=1007`):

```
BENCH S5 stride=150
BENCH S5 fixture_mb=32.6
BENCH S5 fixture_s=2.2
BENCH S5 roots=7
BENCH S5 worktree_badges=6
BENCH S5 c1_ramp_end_rss_kb=97744
BENCH S5 c1_ramp_peak_rss_kb=97744
BENCH S5 c1_ramp_cpu_pct_avg=4.6
BENCH S5 c1_ramp_end_footprint_kb=33792
BENCH S5 c1_ramp_end_reclaimable_kb=60448
BENCH S5 c1_ramp_end_heap_footprint_kb=33485
BENCH S5 c1_ramp_end_heap_inuse_kb=18903
BENCH S5 c1_ramp_end_files=1800
BENCH S5 c1_ramp_end_hunks=36000
BENCH S5 c1_churn_end_rss_kb=91744
BENCH S5 c1_churn_peak_rss_kb=100256
BENCH S5 c1_churn_cpu_pct_avg=6.1
BENCH S5 c1_churn_end_footprint_kb=24576
BENCH S5 c1_churn_end_reclaimable_kb=63696
BENCH S5 c1_churn_end_heap_footprint_kb=24269
BENCH S5 c1_churn_end_heap_inuse_kb=5318
BENCH S5 c1_churn_end_files=296
BENCH S5 c1_churn_end_hunks=5920
BENCH S5 c1_churn_accepts_started=7
BENCH S5 c1_quiet_settle_s=2.6
BENCH S5 c1_quiet_accepts_started=1
BENCH S5 c1_quiet_end_rss_kb=83120
BENCH S5 c1_quiet_peak_rss_kb=93200
BENCH S5 c1_quiet_cpu_pct_avg=1.2
BENCH S5 c1_quiet_end_footprint_kb=24576
BENCH S5 c1_quiet_end_reclaimable_kb=54496
BENCH S5 c1_quiet_end_heap_footprint_kb=24269
BENCH S5 c1_quiet_end_heap_inuse_kb=2498
BENCH S5 c2_ramp_end_rss_kb=95952
BENCH S5 c2_ramp_peak_rss_kb=95952
BENCH S5 c2_ramp_cpu_pct_avg=4.6
BENCH S5 c2_ramp_end_footprint_kb=35840
BENCH S5 c2_ramp_end_reclaimable_kb=56960
BENCH S5 c2_ramp_end_heap_footprint_kb=36045
BENCH S5 c2_ramp_end_heap_inuse_kb=20203
BENCH S5 c2_ramp_end_files=1800
BENCH S5 c2_ramp_end_hunks=36000
BENCH S5 c2_churn_end_rss_kb=89616
BENCH S5 c2_churn_peak_rss_kb=106208
BENCH S5 c2_churn_cpu_pct_avg=6.2
BENCH S5 c2_churn_end_footprint_kb=29696
BENCH S5 c2_churn_end_reclaimable_kb=57280
BENCH S5 c2_churn_end_heap_footprint_kb=30003
BENCH S5 c2_churn_end_heap_inuse_kb=5832
BENCH S5 c2_churn_end_files=214
BENCH S5 c2_churn_end_hunks=4280
BENCH S5 c2_churn_accepts_started=7
BENCH S5 c2_quiet_settle_s=2.5
BENCH S5 c2_quiet_accepts_started=1
BENCH S5 c2_quiet_end_rss_kb=91568
BENCH S5 c2_quiet_peak_rss_kb=92496
BENCH S5 c2_quiet_cpu_pct_avg=1.3
BENCH S5 c2_quiet_end_footprint_kb=30720
BENCH S5 c2_quiet_end_reclaimable_kb=59408
BENCH S5 c2_quiet_end_heap_footprint_kb=30720
BENCH S5 c2_quiet_end_heap_inuse_kb=3780
BENCH S5 c3_ramp_end_rss_kb=95936
BENCH S5 c3_ramp_peak_rss_kb=109888
BENCH S5 c3_ramp_cpu_pct_avg=4.5
BENCH S5 c3_ramp_end_footprint_kb=37888
BENCH S5 c3_ramp_end_reclaimable_kb=56624
BENCH S5 c3_ramp_end_heap_footprint_kb=37683
BENCH S5 c3_ramp_end_heap_inuse_kb=21865
BENCH S5 c3_ramp_end_files=1800
BENCH S5 c3_ramp_end_hunks=36000
BENCH S5 c3_churn_end_rss_kb=120992
BENCH S5 c3_churn_peak_rss_kb=126752
BENCH S5 c3_churn_cpu_pct_avg=8.1
BENCH S5 c3_churn_end_footprint_kb=39936
BENCH S5 c3_churn_end_reclaimable_kb=78992
BENCH S5 c3_churn_end_heap_footprint_kb=39526
BENCH S5 c3_churn_end_heap_inuse_kb=7935
BENCH S5 c3_churn_end_files=244
BENCH S5 c3_churn_end_hunks=4880
BENCH S5 c3_churn_accepts_started=7
BENCH S5 c3_quiet_settle_s=2.4
BENCH S5 c3_quiet_accepts_started=1
BENCH S5 c3_quiet_end_rss_kb=64656
BENCH S5 c3_quiet_peak_rss_kb=123136
BENCH S5 c3_quiet_cpu_pct_avg=1.4
BENCH S5 c3_quiet_end_footprint_kb=37888
BENCH S5 c3_quiet_end_reclaimable_kb=26912
BENCH S5 c3_quiet_end_heap_footprint_kb=38195
BENCH S5 c3_quiet_end_heap_inuse_kb=5299
BENCH S5 prequit_end_rss_kb=89152
BENCH S5 prequit_peak_rss_kb=91504
BENCH S5 prequit_end_files=1365
BENCH S5 prequit_end_hunks=27300
BENCH S5 relaunch_first_frame_s=1.5
BENCH S5 relaunch_peak_rss_kb=218624
BENCH S5 relaunch_peak_rss_kb_1hz=218624
BENCH S5 relaunch_end_rss_kb=218624
BENCH S5 relaunch_cpu_pct_avg=6.0
BENCH S5 relaunch_end_footprint_kb=31744
BENCH S5 relaunch_end_reclaimable_kb=182304
BENCH S5 relaunch_end_heap_footprint_kb=31744
BENCH S5 relaunch_end_heap_inuse_kb=18919
BENCH S5 leaks_count=0
BENCH S5 leaks_bytes=0
BENCH S5 parked_records_max=7
BENCH S5 parked_trees_distinct=42
BENCH S5 writer_switches=42
BENCH S5 writer_failures=0
BENCH S5 wall_s=1007
```

Run K, stride 5 (`wall_s=1011`):

```
BENCH S5 stride=5
BENCH S5 fixture_mb=32.6
BENCH S5 fixture_s=2.8
BENCH S5 roots=7
BENCH S5 worktree_badges=6
BENCH S5 c1_ramp_end_rss_kb=329904
BENCH S5 c1_ramp_peak_rss_kb=329904
BENCH S5 c1_ramp_cpu_pct_avg=4.8
BENCH S5 c1_ramp_end_footprint_kb=335872
BENCH S5 c1_ramp_end_reclaimable_kb=103424
BENCH S5 c1_ramp_end_heap_footprint_kb=336486
BENCH S5 c1_ramp_end_heap_inuse_kb=342039
BENCH S5 c1_ramp_end_files=1800
BENCH S5 c1_ramp_end_hunks=1800
BENCH S5 c1_churn_end_rss_kb=261920
BENCH S5 c1_churn_peak_rss_kb=366816
BENCH S5 c1_churn_cpu_pct_avg=6.4
BENCH S5 c1_churn_end_footprint_kb=63488
BENCH S5 c1_churn_end_reclaimable_kb=205232
BENCH S5 c1_churn_end_heap_footprint_kb=63078
BENCH S5 c1_churn_end_heap_inuse_kb=56815
BENCH S5 c1_churn_end_files=346
BENCH S5 c1_churn_end_hunks=346
BENCH S5 c1_churn_accepts_started=7
BENCH S5 c1_quiet_settle_s=2.5
BENCH S5 c1_quiet_accepts_started=1
BENCH S5 c1_quiet_end_rss_kb=246816
BENCH S5 c1_quiet_peak_rss_kb=262704
BENCH S5 c1_quiet_cpu_pct_avg=1.2
BENCH S5 c1_quiet_end_footprint_kb=63488
BENCH S5 c1_quiet_end_reclaimable_kb=193616
BENCH S5 c1_quiet_end_heap_footprint_kb=63898
BENCH S5 c1_quiet_end_heap_inuse_kb=2515
BENCH S5 c2_ramp_end_rss_kb=342352
BENCH S5 c2_ramp_peak_rss_kb=350000
BENCH S5 c2_ramp_cpu_pct_avg=4.7
BENCH S5 c2_ramp_end_footprint_kb=245760
BENCH S5 c2_ramp_end_reclaimable_kb=126000
BENCH S5 c2_ramp_end_heap_footprint_kb=246067
BENCH S5 c2_ramp_end_heap_inuse_kb=285650
BENCH S5 c2_ramp_end_files=1800
BENCH S5 c2_ramp_end_hunks=1800
BENCH S5 c2_churn_end_rss_kb=242176
BENCH S5 c2_churn_peak_rss_kb=363664
BENCH S5 c2_churn_cpu_pct_avg=7.2
BENCH S5 c2_churn_end_footprint_kb=96256
BENCH S5 c2_churn_end_reclaimable_kb=159824
BENCH S5 c2_churn_end_heap_footprint_kb=96358
BENCH S5 c2_churn_end_heap_inuse_kb=53367
BENCH S5 c2_churn_end_files=316
BENCH S5 c2_churn_end_hunks=316
BENCH S5 c2_churn_accepts_started=7
BENCH S5 c2_quiet_settle_s=2.4
BENCH S5 c2_quiet_accepts_started=1
BENCH S5 c2_quiet_end_rss_kb=145856
BENCH S5 c2_quiet_peak_rss_kb=263456
BENCH S5 c2_quiet_cpu_pct_avg=1.5
BENCH S5 c2_quiet_end_footprint_kb=73728
BENCH S5 c2_quiet_end_reclaimable_kb=97776
BENCH S5 c2_quiet_end_heap_footprint_kb=74138
BENCH S5 c2_quiet_end_heap_inuse_kb=3794
BENCH S5 c3_ramp_end_rss_kb=284320
BENCH S5 c3_ramp_peak_rss_kb=360912
BENCH S5 c3_ramp_cpu_pct_avg=4.8
BENCH S5 c3_ramp_end_footprint_kb=324608
BENCH S5 c3_ramp_end_reclaimable_kb=37216
BENCH S5 c3_ramp_end_heap_footprint_kb=325018
BENCH S5 c3_ramp_end_heap_inuse_kb=286822
BENCH S5 c3_ramp_end_files=1800
BENCH S5 c3_ramp_end_hunks=1800
BENCH S5 c3_churn_end_rss_kb=211248
BENCH S5 c3_churn_peak_rss_kb=374192
BENCH S5 c3_churn_cpu_pct_avg=7.3
BENCH S5 c3_churn_end_footprint_kb=80896
BENCH S5 c3_churn_end_reclaimable_kb=147920
BENCH S5 c3_churn_end_heap_footprint_kb=81101
BENCH S5 c3_churn_end_heap_inuse_kb=26792
BENCH S5 c3_churn_end_files=120
BENCH S5 c3_churn_end_hunks=120
BENCH S5 c3_churn_accepts_started=7
BENCH S5 c3_quiet_settle_s=1.9
BENCH S5 c3_quiet_accepts_started=1
BENCH S5 c3_quiet_end_rss_kb=203888
BENCH S5 c3_quiet_peak_rss_kb=224176
BENCH S5 c3_quiet_cpu_pct_avg=1.4
BENCH S5 c3_quiet_end_footprint_kb=79872
BENCH S5 c3_quiet_end_reclaimable_kb=143600
BENCH S5 c3_quiet_end_heap_footprint_kb=79667
BENCH S5 c3_quiet_end_heap_inuse_kb=5305
BENCH S5 prequit_end_rss_kb=265536
BENCH S5 prequit_peak_rss_kb=266816
BENCH S5 prequit_end_files=1368
BENCH S5 prequit_end_hunks=1368
BENCH S5 relaunch_first_frame_s=1.5
BENCH S5 relaunch_peak_rss_kb=385264
BENCH S5 relaunch_peak_rss_kb_1hz=385264
BENCH S5 relaunch_end_rss_kb=385264
BENCH S5 relaunch_cpu_pct_avg=6.3
BENCH S5 relaunch_end_footprint_kb=91136
BENCH S5 relaunch_end_reclaimable_kb=288768
BENCH S5 relaunch_end_heap_footprint_kb=91136
BENCH S5 relaunch_end_heap_inuse_kb=226784
BENCH S5 leaks_count=0
BENCH S5 leaks_bytes=0
BENCH S5 parked_records_max=7
BENCH S5 parked_trees_distinct=42
BENCH S5 writer_switches=42
BENCH S5 writer_failures=0
BENCH S5 wall_s=1011
```

## Known costs

Not scan costs and not scenarios: things a user can *wait* for that no `BENCH` line
measures. The sponsor asked for this section at the Phase 8 kickoff (ruling P9, 2026-09-06)
so that a cost decided in one phase does not become a mystery in the next. **Every later
phase adds its own rows here.**

### The keyboard-enhancement probe — up to 2 s at launch

`term::enter()` asks the terminal whether it speaks the kitty keyboard protocol before the
input thread starts: crossterm's `supports_keyboard_enhancement()` writes `CSI ? u` followed
by `CSI c` and waits for a reply. A terminal that answers *either* query costs a round trip
(microseconds on a local pty, one network round trip over ssh). **A terminal that answers
neither costs crossterm's full 2 s timeout, before the first frame is drawn.**

| | |
|---|---|
| what it buys | exactly one key: `Shift-Enter` as a newline in the note modal and the inline editor. `Ctrl-J` does the same everywhere, always |
| when it is paid | once per process, before the input thread starts. The answer is a `OnceLock`, so an `$EDITOR` suspend's `term::enter()` re-pushes the flags without re-asking and pays nothing (F8, F18) |
| worst case | 2 s, added to launch, on top of root discovery — which on S1's hundred roots is already ~3.7 s |
| how to skip it | `LASTCALL_KEYBOARD=plain` (an environment switch, deliberately **not** a `[config]` key — §6.1 is frozen). Exactly that spelling; anything else means "ask" |
| who skips it | the PTY harness, in `PtyCommand::isolated_lastcall`, or every e2e scene would pay 2 s |
| failure direction | a probe that errors or times out is a **no**: the flags are never pushed, and the key row promises `^J newline` rather than `⇧⏎` |

Terminals known to answer (so `⇧⏎` works there): kitty, WezTerm, foot, Ghostty, and
iTerm2 ≥ 3.5 with the option enabled. Known **not** to: Terminal.app, tmux without
`extended-keys`, and herdr's own terminal as of the Gate 7 run — which is the case that
matters, because a herdr pane is where lastcall is meant to live.

**The flip, if the sponsor's herdr launch pays the 2 s.** Ruling P9's Alt is a one-line
change in `term.rs`: never probe, and push the flags only when `TERM` names a
kitty-protocol terminal (`xterm-kitty`, `wezterm`, `foot`, `xterm-ghostty`). No query, no
round trip, no timeout — at the price of missing iTerm2 and tmux `extended-keys`, which
report the protocol but do not say so in `TERM`. Two-way door; the measurement the sponsor
takes at the gate is what decides it. **Not measured here**, because the bench harness answers
no queries and would only ever report the worst case.

### Editing, saving and copying — not measured

The Phase 9 editor, the `$EDITOR` round trip and the OSC 52 copy get a row here rather than
a scenario (Phase 9b deliverable 10, ruling P7). They are **not measured**, deliberately:
each one is a human-paced operation over a single file, costing a handful of syscalls and at
most one `git` spawn, so a `BENCH` line would report the harness's own pty latency rather
than anything a user waits for. What each one actually does is short enough to write down.

| | |
|---|---|
| an inline save (`i`, then `^S`) | **one `hash-object` and one rename.** `ops::save_file` hashes the buffer once (`hash-object -w --path=<rel> --stdin`, one `git` spawn) *before* touching the working tree, then `restore::write_bytes` writes a temp file beside the target, `fsync`s it, `fchmod`s it and `renameat`s it into place. The ledger is written the same way afterwards (tmp, `fsync`, rename), so a save is one spawn and two renames, over one file |
| the `$EDITOR` round trip (`I`) | lastcall is suspended for the whole of it: `term::leave()`, the editor's own `status()`, `term::enter()` on return. The keyboard-enhancement answer is a `OnceLock`, so the re-entry re-pushes the flags without re-asking and pays none of the 2 s above (F8, F18). The cost is the editor's, and the confirm on return is one comparison of the bytes |
| a copy (`v`, then `y`) | **one OSC 52 write**, through the same `execute!` path as every other escape sequence the loop writes: `ESC ] 52 ; c ; <base64> BEL`, encoded in-process by `tui::clipboard::base64`. There is no reply to wait for, so there is nothing to time; a selection over `clipboard::CAP` (32 KiB raw, ≈ 43 KiB encoded) is **refused** rather than truncated |
| where it could still get slow | hashing a very large buffer (the one spawn takes the whole file through stdin) and a terminal that is slow to swallow a 43 KiB OSC 52 payload. Neither has been measured, and neither is a scan cost |

### The seen marks' tree cache: bounded by the records it serves

`RootState.seen_cache` (Phase 14 B) holds one `ls-tree -r` listing per tree oid named by a
parked or retired record, shared by every record at that tree. Phase 16 (Amendment v1.18)
adds at most 20 retired listings per root beside the parked ones (`ledger::RETIRED_CAP`),
none older than 30 days at the last prune; each listing is taken once, only when some row
is a candidate, and dropped when no record names its tree any more. Before Phase 16 a deleted
branch's listing left the cache at the switch; now it stays while its record is retired.
Estimated, not measured (no `BENCH` scenario deletes a branch): a listing costs roughly 100
bytes per file, so 20 retired trees of a 5,000-file root are about 10 MB at worst and of a
50,000-file root about 100 MB, and only trees that differ cost anything, since records at
one tree share a listing (the code review's F5).

### Blobs fetched in chunks: one process for each 8 MiB of a first scan

Phase 17 Part B's chunk (`BLOB_CHUNK_BYTES`, 8 MiB) spends git processes to hold fewer blobs
at once: a scan runs one `cat-file --batch` per chunk, so S5's root of about 45 MB of
pending blobs takes about six instead of one, and a file whose two sides pass 8 MiB takes
one of its own. Only rows that changed are fetched, so this falls on a root's first scan
after launch and on what a later scan finds changed, not on every scan. `just bench` S1 to
S4 never reach a second chunk, so no `BENCH` line shows it apart from S5's first frame (1.4
to 1.5 s).

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
