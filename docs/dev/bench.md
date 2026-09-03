# The performance baseline (`just bench`)

Baseline only; targets are set at the Phase 9 kickoff (§8). This page records what
`just bench` printed on one machine on one day so that a later run has something to be
compared against. It is not a gate: nothing here fails a build.

## How the numbers are taken

`just bench` builds the release binary and runs the four `#[ignore]`d scenarios of
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
- `peak_rss_kb` is the binary's resident set as `ps -o rss= -p <pid>` reports it, sampled at
  every `POLL` tick from spawn to exit, maximum kept. No `getrusage`; no new dependency.
- The TUI runs with `--poll 1` in S1, S2 and S4 (and in S3's first half), so head polling
  and the full rescan both tick every second; the `S3_events` half runs with the defaults
  (debounce 750 ms, head poll 10 s, rescan 30 s) so that the number is the watcher's, not
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
(`100 repos · 4000 files`); the RSS peak is read after two further one-second rescans.

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
target candidate, noted here, not acted on.

### S3 — 1,000 files dropped into a clean root under watch

One first-sighted repo with the TUI showing `nothing pending across 1 root` and the status
line at `watching …`; then 1,000 files (`d00`–`d09`, 100 per dir) written as fast as the
harness can. `settle_ms` runs from the last write to `1 repo · 1000 files` in the header.
`S3` is `tui --poll 1`; `S3_events` is `tui` with the default timings (debounce 750 ms, the
30 s rescan backstop), so its number is the watcher's own.

| metric | `S3` (`--poll 1`) | `S3_events` (defaults) |
|---|---|---|
| `files` | 1000 | 1000 |
| `settle_ms` | 1553 | 1553 |
| `peak_rss_kb` | 10592 | 8832 |

### S4 — 50,000 files dropped into a clean root: the row cap

One first-sighted repo under `tui --poll 1`, then 50,000 files (`d00`–`d49`, 1,000 per
dir) written as fast as the harness can (4.7 s, outside the timed region).
`capped_count_ms` runs from the last write to `10000+ files` in the header;
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
harness, not just printed.

## Variance (run B)

Run B, same machine, started right after run A: every `*_ms` value within 1 % of run A
except S3 `settle_ms` (1595 vs 1553, +2.7 %) and the ≤ 15 ms screen metrics (S2 `open_ms`
15 vs 12, i.e. inside the 10 ms `POLL` granularity); `peak_rss_kb` within 6 % (S2 52848 vs
56224, S4 83360 vs 88448); every spawn, row and file count identical.

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
