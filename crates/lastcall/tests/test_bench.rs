//! The Phase 4 performance baseline (kickoff deliverable 10; `docs/dev/bench.md`). Not a
//! gate: five `#[ignore]`d scenarios at the ruled sizes, run only against the release
//! build by
//!
//! ```text
//! just bench
//! ```
//!
//! (`cargo build --release -p lastcall && cargo test --release -p lastcall --test
//! test_bench -- --ignored --nocapture --test-threads=1`). Every scenario writes one
//! `BENCH <scenario> <metric>=<value>` line per metric to stderr; a debug build prints a
//! SKIP line and measures nothing. Fixture construction — the clones, the 100,000-line
//! file, the 1,000- and 50,000-file drops — is outside every timed region; the fixtures
//! live under `TempDir`s that their drops remove.
//!
//! In-process metrics (`*_ms` wall, `spawns` = `lastcall_engine::git::spawn_count` delta)
//! come from an `Engine` over the same state dir the binary then runs on. Screen metrics
//! come from the built binary (`env!("CARGO_BIN_EXE_lastcall")`, the release one under
//! `just bench`) in the PTY harness with `PtyCommand::sample_rss`: `peak_rss_kb` is the
//! child's resident set as `ps -o rss= -p <pid>` reports it, sampled over the scene with a
//! `POLL` (10 ms) sleep between samples — a period of ≈10 ms plus one `ps` — while the
//! screen is read every `POLL`, the granularity of every screen `*_ms` value.
//!
//! S5, the worktree soak (Phase 17 Part A), is the exception to both: it is not part of
//! `just bench` but runs alone under `just bench-soak` (about 17 minutes), and it samples
//! the child once a second on its own thread rather than with the 10 ms sampler.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lastcall_engine::config::Config;
use lastcall_engine::engine::{DEFAULT_ROW_CAP, Engine, EngineOptions, default_parallelism};
use lastcall_engine::git::spawn_count;
use lastcall_testkit::engine::open_engine_with;
use lastcall_testkit::fixture_repo::{FixtureRepo, engine_env_for};
use lastcall_testkit::pty_tui::{PtyCommand, PtyTui, vt100};
use lastcall_testkit::tmp::TempDir;

/// Hard bound on any single screen wait (the 50,000-file drop rescans for a while).
const LONG: Duration = Duration::from_secs(120);
/// S3 `events`: default timings have a 30 s rescan backstop, so a nav that settles later
/// than this was the backstop, not the watcher — reported as a skip.
const EVENTS_BOUND: Duration = Duration::from_secs(25);
/// S2's config: sixteen MiB, so the 1.09 MB file is not collapsed.
const S2_COLLAPSE_SIZE_BYTES: u64 = 16_777_216;
const S2_LINES: usize = 100_000;
const S3_FILES: usize = 1_000;
const S4_FILES: usize = 50_000;

/// `LASTCALL_PARALLELISM` as the built binary reads it, for the in-process metrics.
fn parallelism_env() -> Option<usize> {
    std::env::var_os("LASTCALL_PARALLELISM")?
        .to_str()?
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|n| *n >= 1)
}

fn note(line: &str) {
    let mut err = std::io::stderr();
    let _ = err.write_all(line.as_bytes());
    let _ = err.write_all(b"\n");
    let _ = err.flush();
}

/// A blank line first: under `--nocapture` libtest prints its `test <name> ... ` prefix
/// without a newline, so a scenario's first line would be glued to it and
/// `grep '^BENCH '` would drop it (S2's `file_bytes`, S3's `files`, the SKIP lines).
fn fresh_line() {
    note("");
}

fn bench(scenario: &str, metric: &str, value: impl std::fmt::Display) {
    note(&format!("BENCH {scenario} {metric}={value}"));
}

/// Release only: a debug build measures the allocator, not the product.
fn release_build() -> bool {
    if cfg!(debug_assertions) {
        note("SKIP: bench measures the release build only (run: just bench)");
        return false;
    }
    true
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lastcall"))
}

/// One scenario's world: a parent dir `W` for the roots, a state dir with the config file
/// the binary reads and the `HOME` both engines see. Nothing outside the two temp dirs.
struct Bench {
    _w: TempDir,
    _state: TempDir,
    parent: PathBuf,
    state: PathBuf,
    home: PathBuf,
    config_path: PathBuf,
    config: Config,
}

impl Bench {
    fn new(tag: &str, collapse_size_bytes: Option<u64>) -> Self {
        let w = TempDir::new(&format!("lc-bench-{tag}"));
        let state = TempDir::new(&format!("lc-bench-{tag}-state"));
        let parent = w.join("W");
        std::fs::create_dir_all(&parent).expect("parent dir");
        let home = state.join("home");
        let config_path = state.join("config.toml");
        let mut toml = format!("parent_dirs = [\"{}\"]\n", parent.display());
        let mut config = Config::default();
        if let Some(n) = collapse_size_bytes {
            toml.push_str(&format!("collapse_size_bytes = {n}\n"));
            config.collapse_size_bytes = n;
        }
        std::fs::write(&config_path, toml).expect("config written");
        Bench {
            parent,
            state: state.path().to_path_buf(),
            home,
            config_path,
            config,
            _w: w,
            _state: state,
        }
    }

    /// A fresh `FixtureRepo` at `W/<name>` (its bare origin beside it is not a root).
    fn repo(&self, name: &str) -> FixtureRepo {
        FixtureRepo::new_in(TempDir::adopt(&self.parent), name).expect("fixture repo")
    }

    fn engine(&self) -> Engine {
        let env = engine_env_for(&self.parent, &self.home, &self.state);
        // `LASTCALL_PARALLELISM` reaches the built binary on its own (the PTY inherits the
        // bench's environment); honoring it here too is what makes a whole `just bench` run
        // at width 1 the *before* column for Phase 5 — same machine, same commit, same
        // fixtures, the pool the only difference.
        let options = EngineOptions {
            parallelism: parallelism_env().unwrap_or_else(default_parallelism),
            ..EngineOptions::default()
        };
        open_engine_with(
            &self.parent,
            &env,
            &self.state,
            self.config.clone(),
            options,
        )
    }

    /// First sight of every root, in process, before the scenario's mutation.
    fn first_sight(&self, roots: usize) {
        let mut engine = self.engine();
        let results = engine.scan_all();
        assert_eq!(results.len(), roots, "roots discovered under W");
        for (root, _, result) in &results {
            assert!(
                result.is_ok(),
                "first sight of {}: {result:?}",
                root.display()
            );
        }
    }

    /// The built binary in a 100×30 PTY with RSS sampling on.
    fn tui(&self, args: &[&str]) -> PtyTui {
        PtyCommand::new(bin())
            .cwd(&self.parent)
            .isolated_lastcall(&self.home, &self.config_path, &self.state)
            .args(args)
            .sample_rss()
            .spawn()
            .expect("spawn lastcall tui in a pty")
    }
}

/// The launch hold's counter line shows at least one root reported: `K of N repos
/// checked` with `K` ≥ 1. The counter appears one second into the hold
/// (`Loading::COUNTER_AFTER`), so this form cannot be true earlier than that.
fn first_checked(s: &vt100::Screen) -> bool {
    let text = s.contents();
    text.lines().any(|l| {
        l.trim_start()
            .split_once(" of ")
            .is_some_and(|(k, rest)| rest.contains("repos checked") && k != "0")
    })
}

/// A first root is checked: the counter line says so, **or** the hold is already over and
/// `header` (every root, with its counts) is on screen — every root checked means the first
/// one was.
///
/// Both forms are needed because the hold can be shorter than the counter's one second. The
/// TUI is spawned after the scenario has already opened and scanned the same roots in
/// process, so its own scan runs warm; on a quiet machine that scan finishes before
/// `Loading::COUNTER_AFTER` and the counter line is never drawn at all (run G, 2026-09-11 —
/// the counter-only wait, written at `9d2fd4a` and never run under a bench until then, timed
/// out with the full listing on screen). On a loaded machine the counter appears first and
/// the number floors at ~1,000 ms; on a quiet one it equals `first_frame_ms`.
fn first_checked_or_listed(header: &'static str) -> impl Fn(&vt100::Screen) -> bool {
    move |s: &vt100::Screen| first_checked(s) || s.contents().contains(header)
}

/// The status bar reads `watching …`: the watch is installed and the post-install
/// rescans are done (before that it carries the hint line — the launch hold lives in the
/// pane, Design pass D3).
fn watching(s: &vt100::Screen) -> bool {
    let (_, cols) = s.size();
    s.rows(0, cols)
        .last()
        .is_some_and(|r| r.starts_with("watching "))
}

/// Everything but the status row (whose age ticks on its own).
fn body(s: &vt100::Screen) -> String {
    let (rows, cols) = s.size();
    s.rows(0, cols)
        .take(usize::from(rows) - 1)
        .collect::<Vec<_>>()
        .join("\n")
}

fn quit(pty: &mut PtyTui) {
    pty.send(b"q").expect("q");
    let status = pty
        .wait_exit(Duration::from_secs(10))
        .expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
}

/// `peak_rss_kb`, with a visible note when the harness's sampler gave up before the
/// child exited (the number is then a partial peak, not a frozen one presented as whole).
fn rss(scenario: &str, pty: &PtyTui) {
    if pty.rss_sampler_stopped_early() {
        note(&format!(
            "--- {scenario} peak_rss_kb is PARTIAL: the rss sampler stopped before the child exited ({} failed ps ticks)",
            pty.rss_failed_ticks()
        ));
    }
    bench(
        scenario,
        "peak_rss_kb",
        pty.peak_rss_kb()
            .expect("the harness sampled the child's rss"),
    );
}

/// Write `n` files under `root` (`d<NN>/f<NNNNN>.txt`, `per_dir` per directory, in path
/// order) as fast as `std::fs::write` goes; returns the instant the last write returned.
fn drop_files(root: &Path, n: usize, per_dir: usize) -> Instant {
    for i in 0..n {
        let dir = root.join(format!("d{:02}", i / per_dir));
        if i.is_multiple_of(per_dir) {
            std::fs::create_dir_all(&dir).expect("dir");
        }
        std::fs::write(dir.join(format!("f{i:05}.txt")), format!("file {i}\n")).expect("write");
    }
    Instant::now()
}

/// S1: 100 clones under one parent, 40 committed files each, then every one of those
/// files edited after first sight — 4,000 pending rows.
#[test]
#[ignore]
fn bench_s1_clones_100_rows_4000() {
    fresh_line();
    if !release_build() {
        return;
    }
    const S: &str = "S1";
    let b = Bench::new("s1", None);
    let files: Vec<(String, String)> = (0..40)
        .map(|j| (format!("src/m{j:02}.rs"), format!("fn m{j}() {{}}\n")))
        .collect();
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, c)| (a.as_str(), c.as_str()))
        .collect();
    let built = Instant::now();
    let repos: Vec<FixtureRepo> = (0..100)
        .map(|i| {
            let mut repo = b.repo(&format!("r{i:03}"));
            repo.commit_files(&refs, "forty files").expect("commit");
            repo
        })
        .collect();
    b.first_sight(100);
    for repo in &repos {
        for (path, _) in &files {
            repo.write(path, format!("{path} edited\n"));
        }
    }
    note(&format!(
        "--- S1 fixture: 100 clones × 40 edits built in {:.1?} (not timed)",
        built.elapsed()
    ));

    let before = spawn_count();
    let t = Instant::now();
    let mut engine = b.engine();
    bench(S, "open_ms", t.elapsed().as_millis());
    bench(S, "open_spawns", spawn_count() - before);
    let before = spawn_count();
    let t = Instant::now();
    let results = engine.scan_all();
    let wall = t.elapsed();
    let spawns = spawn_count() - before;
    drop(engine);
    let rows: usize = results
        .iter()
        .map(|(_, _, r)| r.as_ref().map_or(0, |p| p.rows.len()))
        .sum();
    assert_eq!(results.len(), 100);
    assert_eq!(rows, 4_000, "every edit is a row");
    bench(S, "roots", results.len());
    bench(S, "rows", rows);
    bench(S, "scan_all_ms", wall.as_millis());
    bench(S, "scan_all_spawns", spawns);

    // Spawn → a first root checked (the launch hold's counter from one second in, or the
    // hold ending sooner than that), then → every root in the header (the listing lands as
    // one frame).
    let t = Instant::now();
    let mut pty = b.tui(&["tui", "--poll", "1"]);
    pty.wait_for(LONG, first_checked_or_listed("100 repos · 4,000 files"))
        .unwrap_or_else(|e| panic!("first root checked: {e}"));
    bench(S, "first_checked_ms", t.elapsed().as_millis());
    pty.wait_for_text("100 repos · 4,000 files", LONG)
        .unwrap_or_else(|e| panic!("header: {e}"));
    bench(S, "first_frame_ms", t.elapsed().as_millis());
    // Two more `--poll 1` rescans of all 100 roots before the RSS peak is read.
    std::thread::sleep(Duration::from_secs(2));
    quit(&mut pty);
    rss(S, &pty);
}

/// S1h: the same 4,000 pending rows as S1, but as **50 clones of 80 files** instead of 100
/// of 40 — half the roots, twice the work each. S1 measures the per-root overhead (a root
/// is a fixed number of git spawns however small it is); S1h holds the row count fixed and
/// halves the number of roots, so the two together separate per-root cost from per-row
/// cost, and a pool that only helps when there are many tiny roots shows up as S1
/// improving while S1h does not (Phase 5 deliverable 1d).
#[test]
#[ignore]
fn bench_s1h_clones_50_files_80_rows_4000() {
    fresh_line();
    if !release_build() {
        return;
    }
    const S: &str = "S1h";
    let b = Bench::new("s1h", None);
    let files: Vec<(String, String)> = (0..80)
        .map(|j| (format!("src/m{j:02}.rs"), format!("fn m{j}() {{}}\n")))
        .collect();
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, c)| (a.as_str(), c.as_str()))
        .collect();
    let built = Instant::now();
    let repos: Vec<FixtureRepo> = (0..50)
        .map(|i| {
            let mut repo = b.repo(&format!("r{i:03}"));
            repo.commit_files(&refs, "eighty files").expect("commit");
            repo
        })
        .collect();
    b.first_sight(50);
    for repo in &repos {
        for (path, _) in &files {
            repo.write(path, format!("{path} edited\n"));
        }
    }
    note(&format!(
        "--- S1h fixture: 50 clones × 80 edits built in {:.1?} (not timed)",
        built.elapsed()
    ));

    let before = spawn_count();
    let t = Instant::now();
    let mut engine = b.engine();
    bench(S, "open_ms", t.elapsed().as_millis());
    bench(S, "open_spawns", spawn_count() - before);
    let before = spawn_count();
    let t = Instant::now();
    let results = engine.scan_all();
    let wall = t.elapsed();
    let spawns = spawn_count() - before;
    drop(engine);
    let rows: usize = results
        .iter()
        .map(|(_, _, r)| r.as_ref().map_or(0, |p| p.rows.len()))
        .sum();
    assert_eq!(results.len(), 50);
    assert_eq!(rows, 4_000, "every edit is a row");
    bench(S, "roots", results.len());
    bench(S, "rows", rows);
    bench(S, "scan_all_ms", wall.as_millis());
    bench(S, "scan_all_spawns", spawns);

    let t = Instant::now();
    let mut pty = b.tui(&["tui", "--poll", "1"]);
    pty.wait_for(LONG, first_checked_or_listed("50 repos · 4,000 files"))
        .unwrap_or_else(|e| panic!("first root checked: {e}"));
    bench(S, "first_checked_ms", t.elapsed().as_millis());
    pty.wait_for_text("50 repos · 4,000 files", LONG)
        .unwrap_or_else(|e| panic!("header: {e}"));
    bench(S, "first_frame_ms", t.elapsed().as_millis());
    std::thread::sleep(Duration::from_secs(2));
    quit(&mut pty);
    rss(S, &pty);
}

/// S2: one committed 100,000-line file rewritten so every line changes except lines
/// 50,000–50,010: two hunks under `collapse_size_bytes = 16777216`, one collapsed row
/// under the default config.
#[test]
#[ignore]
fn bench_s2_diff_100k_lines() {
    fresh_line();
    if !release_build() {
        return;
    }
    const S: &str = "S2";
    let original: String = (1..=S2_LINES).map(|i| format!("line {i:05}\n")).collect();
    let rewritten: String = (1..=S2_LINES)
        .map(|i| {
            if (50_000..=50_010).contains(&i) {
                format!("line {i:05}\n")
            } else {
                format!("LINE {i:05}\n")
            }
        })
        .collect();
    bench(S, "file_bytes", original.len());

    let b = Bench::new("s2", Some(S2_COLLAPSE_SIZE_BYTES));
    let mut repo = b.repo("big");
    repo.commit_files(&[("big.txt", original.as_str())], "100k lines")
        .expect("commit");
    b.first_sight(1);
    repo.write("big.txt", &rewritten);

    let mut engine = b.engine();
    let root = engine.root_paths()[0].clone();
    let before = spawn_count();
    let t = Instant::now();
    let pile = engine.scan(&root).expect("scan");
    let wall = t.elapsed();
    let spawns = spawn_count() - before;
    drop(engine);
    assert_eq!(pile.rows.len(), 1, "{pile:?}");
    assert_eq!(pile.rows[0].hunks.len(), 2, "two hunks");
    bench(S, "hunks", pile.rows[0].hunks.len());
    bench(S, "scan_ms", wall.as_millis());
    bench(S, "scan_spawns", spawns);

    let mut pty = b.tui(&["tui", "--poll", "1"]);
    // `M big` and not `M big.txt`: `+99,989 −99,989` (thousands separators, ruling 2)
    // leaves the 28-column nav no room for the extension, so the name renders `big.…`.
    pty.wait_for_text("M big", LONG)
        .unwrap_or_else(|e| panic!("the row: {e}"));
    let t = Instant::now();
    pty.send(b"jj\r").expect("open");
    pty.wait_for(LONG, |s| s.contents().contains("@@ -1,"))
        .unwrap_or_else(|e| panic!("first hunk header: {e}"));
    bench(S, "open_ms", t.elapsed().as_millis());
    let t = Instant::now();
    pty.send(b"n").expect("n");
    pty.wait_for(LONG, |s| {
        let (_, cols) = s.size();
        s.rows(0, cols).enumerate().any(|(i, r)| {
            r.contains("@@ -")
                && !r.contains("@@ -1,")
                && lastcall_testkit::pty_tui::col_of(&r, "@@ -")
                    .and_then(|c| s.cell(i as u16, c))
                    .is_some_and(vt100::Cell::inverse)
        })
    })
    .unwrap_or_else(|e| panic!("second hunk header inverted: {e}"));
    bench(S, "hunk_next_ms", t.elapsed().as_millis());
    let was = pty.screen(body);
    let t = Instant::now();
    pty.send(b"\x1b[6~").expect("page down");
    pty.wait_for(LONG, |s| body(s) != was)
        .unwrap_or_else(|e| panic!("page down: {e}"));
    bench(S, "page_down_ms", t.elapsed().as_millis());
    quit(&mut pty);
    rss(S, &pty);

    // The default config: the same file is over the 512 KiB collapse size — one collapsed
    // row, no hunks; `open_ms` is to the collapsed placeholder.
    let b = Bench::new("s2-default", None);
    let mut repo = b.repo("big");
    repo.commit_files(&[("big.txt", original.as_str())], "100k lines")
        .expect("commit");
    b.first_sight(1);
    repo.write("big.txt", &rewritten);
    let mut pty = b.tui(&["tui", "--poll", "1"]);
    pty.wait_for_text("⊟", LONG)
        .unwrap_or_else(|e| panic!("the row: {e}"));
    let t = Instant::now();
    pty.send(b"jj\r").expect("open");
    pty.wait_for_text("collapsed (", LONG)
        .unwrap_or_else(|e| panic!("collapsed placeholder: {e}"));
    bench("S2_default_config", "open_ms", t.elapsed().as_millis());
    quit(&mut pty);
}

/// S3: the TUI up on a clean root, then 1,000 new files as fast as the fixture writes
/// them; `settle_ms` from the last write returning to the header reading `1,000 files`.
/// Twice: `--poll 1` (the 1 s rescan backstop) and default timings (`events`: the
/// filesystem watcher, whose line is a skip when nothing fires within 25 s).
#[test]
#[ignore]
fn bench_s3_burst_1000_under_watch() {
    fresh_line();
    if !release_build() {
        return;
    }
    for (scenario, args, bound) in [
        ("S3", &["tui", "--poll", "1"][..], LONG),
        ("S3_events", &["tui"][..], EVENTS_BOUND),
    ] {
        let b = Bench::new("s3", None);
        let repo = b.repo("burst");
        b.first_sight(1);
        let mut pty = b.tui(args);
        pty.wait_for(LONG, watching)
            .unwrap_or_else(|e| panic!("watching: {e}"));
        assert!(pty.screen_text().contains("nothing pending across 1 repo"));
        let last_write = drop_files(repo.path(), S3_FILES, 100);
        match pty.wait_for(bound, |s| s.contents().contains("1 repo · 1,000 files")) {
            Ok(_) => {
                bench(scenario, "files", S3_FILES);
                bench(scenario, "settle_ms", last_write.elapsed().as_millis());
            }
            Err(e) if scenario == "S3_events" => {
                note(&format!(
                    "SKIP: BENCH {scenario} settle_ms — the filesystem watcher never delivered the burst within {bound:?} (the 30 s rescan backstop would; FSEvents unhealthy on this host): {}",
                    e.lines().next().unwrap_or("")
                ));
            }
            Err(e) => panic!("{scenario}: {e}"),
        }
        quit(&mut pty);
        rss(scenario, &pty);
    }
}

/// S4: 50,000 files dropped into one otherwise clean root under watch: the row cap.
/// `settle_ms` runs from the last write to the notice with the final numbers on screen
/// (`j` selects the root once it is listed — the notice is in the root's main view);
/// then the in-process scan gives `rows_shown` / `omitted` exactly.
#[test]
#[ignore]
fn bench_s4_drop_50000_cutoff() {
    fresh_line();
    if !release_build() {
        return;
    }
    const S: &str = "S4";
    let b = Bench::new("s4", None);
    let repo = b.repo("drop");
    b.first_sight(1);
    let mut pty = b.tui(&["tui", "--poll", "1"]);
    pty.wait_for(LONG, watching)
        .unwrap_or_else(|e| panic!("watching: {e}"));
    assert!(pty.screen_text().contains("nothing pending across 1 repo"));
    let built = Instant::now();
    let last_write = drop_files(repo.path(), S4_FILES, 1_000);
    note(&format!(
        "--- S4 fixture: {S4_FILES} files written in {:.1?} (not timed)",
        built.elapsed()
    ));
    let cap = DEFAULT_ROW_CAP;
    let omitted = S4_FILES - cap;
    let notice = format!(
        "{} files shown · {} more changed paths not scanned (first {} by path)",
        thousands(cap),
        thousands(omitted),
        thousands(cap)
    );
    pty.wait_for_text(&format!("{}+ files", thousands(cap)), LONG)
        .unwrap_or_else(|e| panic!("capped count: {e}"));
    bench(S, "capped_count_ms", last_write.elapsed().as_millis());
    pty.send(b"j").expect("select the root");
    pty.wait_for_text(&notice, LONG)
        .unwrap_or_else(|e| panic!("the notice: {e}"));
    bench(S, "settle_ms", last_write.elapsed().as_millis());
    note(&format!("--- S4 notice on screen: {notice}"));
    quit(&mut pty);
    rss(S, &pty);

    let mut engine = b.engine();
    let root = engine.root_paths()[0].clone();
    let before = spawn_count();
    let t = Instant::now();
    let pile = engine.scan(&root).expect("scan");
    let wall = t.elapsed();
    let spawns = spawn_count() - before;
    drop(engine);
    assert_eq!(pile.rows.len(), cap, "rows_shown == cap");
    assert_eq!(pile.omitted, omitted, "omitted == 50000 - cap");
    assert!(
        pile.notices.iter().any(|n| n == &notice),
        "{:?}",
        pile.notices
    );
    bench(S, "scan_ms", wall.as_millis());
    bench(S, "scan_spawns", spawns);
    bench(S, "rows_shown", pile.rows.len());
    bench(S, "omitted", pile.omitted);
}

/// `10000` → `10,000` (the notice's format).
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

// ---- S5: the worktree soak (Phase 17 Part A, docs/spec/170-phase17-kickoff.md) ----------

/// Committed text files, and lines in each: 34.2 MB (32.6 MiB) of text per checkout, so one
/// root's pending batch is tens of MB (`LASTCALL_BENCH_S5_FILES` / `_LINES` override both).
const S5_FILES: usize = 300;
const S5_LINES: usize = 3_000;
/// A write edits every line whose number is `seq` modulo this: 20 lines of a 3,000-line
/// file, 150 apart, so 20 separate hunks under the diff's 3 lines of context
/// (`LASTCALL_BENCH_S5_STRIDE` overrides it; at 5 the edits merge into one hunk per file).
const S5_STRIDE: usize = 150;
/// Linked worktrees beside the repository (`w01` … on `agent/w01` …), one writer each.
const S5_WORKTREES: usize = 6;
/// A writer's period: one file rewritten per tick.
const S5_WRITE: Duration = Duration::from_millis(250);
/// Churn-phase writes between a writer's commit-and-switch: about a minute at `S5_WRITE`.
/// Scaled with the phases, so a short rehearsal still parks records.
const S5_BRANCH_EVERY: u64 = 240;
const S5_CYCLES: u32 = 3;
/// The phase lengths; `LASTCALL_BENCH_S5_SCALE` multiplies every one of them.
const S5_RAMP: Duration = Duration::from_secs(90);
const S5_CHURN: Duration = Duration::from_secs(150);
const S5_QUIET: Duration = Duration::from_secs(60);
const S5_RELAUNCH_WRITE: Duration = Duration::from_secs(60);
const S5_RELAUNCH_SAMPLE: Duration = Duration::from_secs(30);
/// The churn phase's `ctrl-a` cadence: the reader's, so not scaled.
const S5_ACCEPT_EVERY: Duration = Duration::from_secs(20);
const S5_SAMPLE_EVERY: Duration = Duration::from_secs(1);
/// One `trace` line per this many seconds of the run.
const S5_TRACE_EVERY: u64 = 10;
/// How long `ctrl-a` gets to open the confirm (or to start or refuse an accept), and how
/// long `y` gets to close it.
const S5_MODAL_WAIT: Duration = Duration::from_secs(3);
/// Every macOS memory tool is time-boxed to this; a timeout prints `n/a`.
const S5_TOOL_TIMEOUT: Duration = Duration::from_secs(30);
/// Quiet: the header must hold still this long (at most half the phase) before the final
/// `ctrl-a`, so the accept covers the writers' last writes rather than racing their scans.
const S5_QUIET_STILL: Duration = Duration::from_secs(5);
/// Quiet: while rows remain after the final accept, `ctrl-a` again this often.
const S5_QUIET_RETRY: Duration = Duration::from_secs(5);
/// The confirm's question, `Accept all <N> files in <root>?` / `… across <R> repos?`.
const S5_MODAL: &str = "Accept all ";
/// `tui::app::ACCEPT_IN_PROGRESS` and `NOTHING_TO_ACCEPT`, spelled out (the binary's
/// crate is not a dependency of its own tests' harness).
const S5_REFUSED: [&str; 2] = ["accept in progress", "nothing to accept"];

/// The three knobs, read the way [`parallelism_env`] reads its own.
struct S5Knobs {
    scale: f64,
    files: usize,
    lines: usize,
    stride: usize,
}

fn env_number<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var_os(name)?.to_str()?.trim().parse::<T>().ok()
}

impl S5Knobs {
    fn from_env() -> Self {
        S5Knobs {
            scale: env_number::<f64>("LASTCALL_BENCH_S5_SCALE")
                .filter(|s| s.is_finite() && *s > 0.0)
                .unwrap_or(1.0),
            files: env_number::<usize>("LASTCALL_BENCH_S5_FILES")
                .filter(|n| *n >= 1)
                .unwrap_or(S5_FILES),
            lines: env_number::<usize>("LASTCALL_BENCH_S5_LINES")
                .filter(|n| *n >= 1)
                .unwrap_or(S5_LINES),
            stride: env_number::<usize>("LASTCALL_BENCH_S5_STRIDE")
                .filter(|n| *n >= 1)
                .unwrap_or(S5_STRIDE),
        }
    }

    fn phase(&self, length: Duration) -> Duration {
        length.mul_f64(self.scale)
    }

    fn branch_every(&self) -> u64 {
        ((S5_BRANCH_EVERY as f64 * self.scale).round() as u64).max(1)
    }
}

/// One whole line to stderr under the lock: the sampler thread and the test's thread both
/// print, and [`note`]'s two writes could interleave.
fn s5_line(line: &str) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(format!("{line}\n").as_bytes());
    let _ = err.flush();
}

fn s5(metric: &str, value: impl std::fmt::Display) {
    s5_line(&format!("BENCH S5 {metric}={value}"));
}

fn s5_opt(metric: &str, value: Option<impl std::fmt::Display>) {
    match value {
        Some(v) => s5(metric, v),
        None => s5(metric, "n/a"),
    }
}

fn s5_file(f: usize) -> String {
    format!("f{f:03}.txt")
}

/// File `f`'s text. `edit` = `(writer, seq)` rewrites every line whose number is `seq`
/// modulo `stride` with a line naming both: one line in `stride`, a different set and a new
/// stamp at every write, so each rewrite changes content already pending.
fn s5_text(f: usize, lines: usize, stride: usize, edit: Option<(usize, u64)>) -> String {
    let stride = stride.max(1) as u64;
    let mut out = String::with_capacity(lines * 40);
    for j in 0..lines {
        let line = match edit {
            Some((w, seq)) if j as u64 % stride == seq % stride => {
                format!("f{f:03} l{j:04} w{w:02} edit {seq:06} ipsum\n")
            }
            _ => format!("f{f:03} l{j:04} lorem ipsum dolor sit amet\n"),
        };
        out.push_str(&line);
    }
    out
}

/// One worktree's writer, kept across cycles so the round-robin and the branch count go on.
struct S5Writer {
    index: usize,
    dir: PathBuf,
    seq: u64,
    writes: u64,
    churn_writes: u64,
    switches: u64,
    failures: u64,
}

/// Rewrite the next file every [`S5_WRITE`] until `stop`; while `churn` holds, every
/// `branch_every` writes commit and `git switch -c` to a fresh branch.
fn s5_write_loop(
    repo: &FixtureRepo,
    w: &mut S5Writer,
    knobs: &S5Knobs,
    stop: &AtomicBool,
    churn: &AtomicBool,
) {
    let branch_every = knobs.branch_every();
    while !stop.load(Ordering::Relaxed) {
        let tick = Instant::now();
        let f = (w.seq % knobs.files as u64) as usize;
        w.seq += 1;
        let text = s5_text(f, knobs.lines, knobs.stride, Some((w.index, w.seq)));
        if let Err(e) = std::fs::write(w.dir.join(s5_file(f)), text) {
            w.failures += 1;
            s5_line(&format!(
                "--- S5 writer w{:02}: write {} failed: {e}",
                w.index,
                s5_file(f)
            ));
        }
        w.writes += 1;
        if churn.load(Ordering::Relaxed) {
            w.churn_writes += 1;
            if w.churn_writes.is_multiple_of(branch_every) {
                s5_switch(repo, w);
            }
        }
        while !stop.load(Ordering::Relaxed) {
            let left = S5_WRITE.saturating_sub(tick.elapsed());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
    }
}

/// Commit what the worktree has and switch to `agent/wNN-<k>`, leaving the old branch in
/// place. A git failure is printed and the writer carries on.
fn s5_switch(repo: &FixtureRepo, w: &mut S5Writer) {
    let k = w.switches + 1;
    let message = format!("w{:02} checkpoint {k}", w.index);
    let branch = format!("agent/w{:02}-{k}", w.index);
    let steps: [&[&str]; 3] = [
        &["add", "-A"],
        &["commit", "-q", "-m", &message],
        &["switch", "-q", "-c", &branch],
    ];
    for args in steps {
        if let Err(e) = repo.git_at(&w.dir, args) {
            w.failures += 1;
            let lock = if e.stderr.contains("index.lock") {
                " (an index.lock: lastcall never writes the user's index, so this is news)"
            } else {
                ""
            };
            s5_line(&format!(
                "--- S5 writer w{:02}: git {} failed{lock}: {}",
                w.index,
                args.join(" "),
                e.stderr.trim()
            ));
            return;
        }
    }
    w.switches += 1;
}

/// `ps -o rss=,time= -p <pid>`: KiB and cumulative CPU seconds; `None` once it is gone.
fn s5_ps(pid: u32) -> Option<(u64, f64)> {
    let out = Command::new("ps")
        .args(["-o", "rss=,time=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_ps_sample(&String::from_utf8_lossy(&out.stdout))
}

fn s5_fmt_pct(pct: Option<f64>) -> String {
    pct.map_or_else(|| "n/a".to_owned(), |p| format!("{p:.1}"))
}

/// What the sampler thread shares with the test: the label it prints, the phase's peak
/// and the last sample.
#[derive(Default)]
struct S5Live {
    cycle: u32,
    phase: &'static str,
    peak_kb: u64,
    last_kb: Option<u64>,
    last_cpu_pct: Option<f64>,
    failed: u64,
}

/// The once-a-second sampler over one child: resident set and CPU time from one `ps`, CPU
/// percent from the time deltas (not `ps -o %cpu`, a decaying average on macOS), and one
/// `trace` line every [`S5_TRACE_EVERY`] seconds of the run.
struct S5Sampler {
    pid: u32,
    t0: Instant,
    stop: Arc<AtomicBool>,
    live: Arc<Mutex<S5Live>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// A phase as it began: the label, the instant and the child's CPU seconds then.
struct S5Phase {
    cycle: u32,
    name: &'static str,
    at: Instant,
    cpu_s: Option<f64>,
}

struct S5PhaseEnd {
    end_kb: Option<u64>,
    peak_kb: Option<u64>,
    cpu_pct_avg: Option<f64>,
}

fn s5_lock(live: &Mutex<S5Live>) -> MutexGuard<'_, S5Live> {
    live.lock().unwrap_or_else(|e| e.into_inner())
}

fn s5_sample_loop(pid: u32, t0: Instant, stop: &AtomicBool, live: &Mutex<S5Live>) {
    let mut prev: Option<(Instant, f64)> = None;
    let mut next_trace = 0u64;
    while !stop.load(Ordering::Relaxed) {
        let tick = Instant::now();
        match s5_ps(pid) {
            Some((kb, cpu)) => {
                let pct = prev.and_then(|(at, was)| {
                    let dt = tick.duration_since(at).as_secs_f64();
                    (dt > 0.0).then(|| (cpu - was).max(0.0) / dt * 100.0)
                });
                prev = Some((tick, cpu));
                let mut l = s5_lock(live);
                l.peak_kb = l.peak_kb.max(kb);
                l.last_kb = Some(kb);
                if pct.is_some() {
                    l.last_cpu_pct = pct;
                }
                let t = tick.duration_since(t0).as_secs();
                if t >= next_trace {
                    s5_line(&format!(
                        "BENCH S5 trace t={t} cycle={} phase={} rss_kb={kb} cpu_pct={}",
                        l.cycle,
                        l.phase,
                        s5_fmt_pct(pct)
                    ));
                    next_trace = (t / S5_TRACE_EVERY + 1) * S5_TRACE_EVERY;
                }
            }
            None => s5_lock(live).failed += 1,
        }
        while !stop.load(Ordering::Relaxed) {
            let left = S5_SAMPLE_EVERY.saturating_sub(tick.elapsed());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
    }
}

impl S5Sampler {
    fn start(pid: u32, t0: Instant, cycle: u32, phase: &'static str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let live = Arc::new(Mutex::new(S5Live {
            cycle,
            phase,
            ..S5Live::default()
        }));
        let thread = {
            let (stop, live) = (stop.clone(), live.clone());
            std::thread::Builder::new()
                .name("lastcall-s5-sampler".to_owned())
                .spawn(move || s5_sample_loop(pid, t0, &stop, &live))
                .expect("the sampler thread")
        };
        S5Sampler {
            pid,
            t0,
            stop,
            live,
            thread: Some(thread),
        }
    }

    /// Label the samples from now on and restart the peak at a sample taken now.
    fn begin(&self, cycle: u32, name: &'static str) -> S5Phase {
        let sample = s5_ps(self.pid);
        {
            let mut l = s5_lock(&self.live);
            l.cycle = cycle;
            l.phase = name;
            l.peak_kb = sample.map_or(0, |s| s.0);
        }
        S5Phase {
            cycle,
            name,
            at: Instant::now(),
            cpu_s: sample.map(|s| s.1),
        }
    }

    /// A sample taken now closes the phase: its RSS is the end, the peak is the larger of
    /// it and every sample since `begin`, and the CPU average is the time delta over the wall.
    fn end(&self, phase: &S5Phase) -> S5PhaseEnd {
        let sample = s5_ps(self.pid);
        let wall = phase.at.elapsed().as_secs_f64();
        let peak = s5_lock(&self.live).peak_kb.max(sample.map_or(0, |s| s.0));
        let cpu_pct_avg = match (phase.cpu_s, sample) {
            (Some(was), Some((_, now))) if wall > 0.0 => Some((now - was).max(0.0) / wall * 100.0),
            _ => None,
        };
        S5PhaseEnd {
            end_kb: sample.map(|s| s.0),
            peak_kb: (peak > 0).then_some(peak),
            cpu_pct_avg,
        }
    }

    /// A `trace` line now, from the last sample, with `event=<event>`.
    fn event(&self, event: &str) {
        let l = s5_lock(&self.live);
        s5_line(&format!(
            "BENCH S5 trace t={} cycle={} phase={} rss_kb={} cpu_pct={} event={event}",
            self.t0.elapsed().as_secs(),
            l.cycle,
            l.phase,
            l.last_kb
                .map_or_else(|| "n/a".to_owned(), |kb| kb.to_string()),
            s5_fmt_pct(l.last_cpu_pct)
        ));
    }

    /// Stop and join the thread; the count of `ps` ticks that gave no number.
    fn finish(&mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        s5_lock(&self.live).failed
    }
}

impl Drop for S5Sampler {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Run one macOS memory tool against the child, its output to a file (the `leaks` report
/// can be long, and a pipe nobody drains would stall it), time-boxed to
/// [`S5_TOOL_TIMEOUT`]. `None` off macOS, when the tool is absent, or on a timeout.
fn s5_tool(dir: &Path, tool: &str, args: &[&str]) -> Option<String> {
    static RUNS: AtomicU64 = AtomicU64::new(0);
    if !cfg!(target_os = "macos") {
        return None;
    }
    let path = dir.join(format!(
        "{tool}-{}.txt",
        RUNS.fetch_add(1, Ordering::Relaxed)
    ));
    let file = std::fs::File::create(&path).ok()?;
    let mut child = Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    let finished = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if start.elapsed() < S5_TOOL_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let text = std::fs::read(&path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let _ = std::fs::remove_file(&path);
    if !finished {
        s5_line(&format!(
            "--- S5 {tool} did not finish within {S5_TOOL_TIMEOUT:?}: n/a"
        ));
        return None;
    }
    text
}

/// `footprint` and `heap` at a phase's end, all in KiB. The two tools run one after the
/// other, seconds apart on a loaded phase while the writers still write, so only
/// `heap`'s own pair (`heap_footprint`, `heap_inuse`, read while `heap` holds the task
/// suspended) is one instant; `footprint`'s run gives the total and `Reclaimable`.
struct S5Memory {
    footprint: Option<u64>,
    reclaimable: Option<u64>,
    heap_footprint: Option<u64>,
    heap_inuse: Option<u64>,
}

fn s5_memory(dir: &Path, pid: u32) -> S5Memory {
    let pid = pid.to_string();
    let footprint = s5_tool(dir, "footprint", &[&pid]).and_then(|t| parse_footprint(&t));
    let heap = s5_tool(dir, "heap", &[&pid]);
    S5Memory {
        footprint: footprint.map(|f| f.0),
        reclaimable: footprint.map(|f| f.1),
        heap_footprint: heap.as_deref().and_then(parse_heap_footprint_kb),
        heap_inuse: heap.as_deref().and_then(parse_heap_inuse_kb),
    }
}

/// `<prefix>_end_footprint_kb` … `<prefix>_end_heap_inuse_kb`.
fn s5_memory_lines(prefix: &str, m: &S5Memory) {
    s5_opt(&format!("{prefix}_end_footprint_kb"), m.footprint);
    s5_opt(&format!("{prefix}_end_reclaimable_kb"), m.reclaimable);
    s5_opt(&format!("{prefix}_end_heap_footprint_kb"), m.heap_footprint);
    s5_opt(&format!("{prefix}_end_heap_inuse_kb"), m.heap_inuse);
}

fn s5_header(s: &vt100::Screen) -> Option<(usize, usize)> {
    let (_, cols) = s.size();
    s.rows(0, cols)
        .next()
        .and_then(|row| parse_header_counts(&row))
}

fn s5_status(s: &vt100::Screen) -> String {
    let (_, cols) = s.size();
    s.rows(0, cols).last().unwrap_or_default()
}

fn s5_modal(s: &vt100::Screen) -> bool {
    let (_, cols) = s.size();
    s.rows(0, cols)
        .any(|row| row.contains(S5_MODAL) && row.contains('?'))
}

/// Nothing pending: the empty state, or a header that counts no file.
fn s5_settled(s: &vt100::Screen) -> bool {
    s.contents().contains("nothing pending across") || s5_header(s).is_some_and(|(f, _)| f == 0)
}

fn s5_alive(pty: &PtyTui) {
    assert!(
        !pty.eof(),
        "lastcall exited during the soak; screen:\n{}",
        pty.screen_text()
    );
}

/// Wait until `until`, checking every 100 ms that the child is still there.
fn s5_idle(pty: &PtyTui, until: Instant) {
    loop {
        s5_alive(pty);
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
}

/// `ctrl-a`, then `y` **only** once the confirm's question is on screen: a blind `y` is
/// `copy`, and at ten files or fewer (`CONFIRM_ABOVE`) there is no confirm at all.
/// True when an accept started: the confirm was answered, or the status line newly reads
/// `accepting…` (no confirm under eleven files). A refusal (`accept in progress`, `nothing
/// to accept`) or no sign within [`S5_MODAL_WAIT`] is false; the harness never blocks on a
/// modal that `ctrl-a` with nothing listed never opens.
fn s5_accept(pty: &mut PtyTui) -> bool {
    let before = pty.screen(s5_status);
    let fresh =
        |s: &vt100::Screen, needle: &str| !before.contains(needle) && s5_status(s).contains(needle);
    pty.send(b"\x01").expect("ctrl-a");
    let deadline = Instant::now() + S5_MODAL_WAIT;
    while Instant::now() < deadline {
        let (modal, started, refused) = pty.screen(|s| {
            (
                s5_modal(s),
                fresh(s, "accepting…"),
                S5_REFUSED.iter().any(|r| fresh(s, r)),
            )
        });
        if modal {
            pty.send(b"y").expect("y");
            let _ = pty.wait_for(S5_MODAL_WAIT, |s| !s5_modal(s));
            return true;
        }
        if started {
            return true;
        }
        if refused {
            return false;
        }
        std::thread::sleep(lastcall_testkit::pty_tui::POLL);
    }
    false
}

/// Close a phase: the end and peak RSS, the CPU average, the header's counts when asked,
/// then `footprint` and `heap` (each pauses the child briefly, after the sample).
fn s5_phase_end(sampler: &S5Sampler, pty: &PtyTui, phase: &S5Phase, tools: &Path, counts: bool) {
    let end = sampler.end(phase);
    let header = counts.then(|| pty.screen(s5_header));
    let memory = s5_memory(tools, sampler.pid);
    let p = format!("c{}_{}", phase.cycle, phase.name);
    s5_opt(&format!("{p}_end_rss_kb"), end.end_kb);
    s5_opt(&format!("{p}_peak_rss_kb"), end.peak_kb);
    s5_opt(
        &format!("{p}_cpu_pct_avg"),
        end.cpu_pct_avg.map(|c| format!("{c:.1}")),
    );
    s5_memory_lines(&p, &memory);
    if let Some(header) = header {
        s5_opt(&format!("{p}_end_files"), header.map(|h| h.0));
        s5_opt(&format!("{p}_end_hunks"), header.map(|h| h.1));
    }
}

/// The quiet phase after the writers are joined: let the header hold still, send the
/// final `ctrl-a` (+ `y`), then `ctrl-a` again every [`S5_QUIET_RETRY`] while rows remain
/// (rows the first accept's pile predates), until nothing is pending or the phase ends.
fn s5_quiet(sampler: &S5Sampler, pty: &mut PtyTui, phase: &S5Phase, length: Duration) {
    let until = phase.at + length;
    let still_by = phase.at + length / 2;
    let mut last = pty.screen(s5_header);
    let mut since = Instant::now();
    while Instant::now() < still_by && since.elapsed() < S5_QUIET_STILL {
        s5_idle(
            pty,
            still_by.min(Instant::now() + Duration::from_millis(200)),
        );
        let now = pty.screen(s5_header);
        if now != last {
            last = now;
            since = Instant::now();
        }
    }
    let mut first: Option<Instant> = None;
    let mut started = 0u32;
    let mut settled: Option<Instant> = None;
    let mut next_try = Instant::now();
    while Instant::now() < until {
        if pty.screen(s5_settled) {
            settled = Some(Instant::now());
            break;
        }
        if Instant::now() >= next_try {
            let sent = Instant::now();
            if s5_accept(pty) {
                started += 1;
                first.get_or_insert(sent);
                sampler.event("accept");
            }
            next_try = Instant::now() + S5_QUIET_RETRY;
        }
        s5_idle(pty, until.min(Instant::now() + Duration::from_millis(100)));
    }
    let metric = format!("c{}_quiet_settle_s", phase.cycle);
    match (settled, first) {
        (Some(at), Some(from)) => s5(&metric, format!("{:.1}", (at - from).as_secs_f64())),
        (Some(_), None) => {
            s5(&metric, "0.0");
            s5_line("--- S5 nothing was pending when the quiet phase looked: no final accept");
        }
        (None, _) => {
            s5(&metric, "SKIP");
            s5_line(&format!(
                "--- S5 c{} quiet never settled within {length:?} ({} accepts started); header: {:?}",
                phase.cycle,
                started,
                pty.screen(s5_header)
            ));
        }
    }
    s5(&format!("c{}_quiet_accepts_started", phase.cycle), started);
    s5_idle(pty, until);
}

/// `sample_rss` turns on the harness's 10 ms RSS sampler: the relaunch only, where the
/// launch's `scan_all` peak lasts about a second and the 1 s sampler can miss it.
fn s5_spawn(b: &Bench, sample_rss: bool) -> PtyTui {
    let cmd = PtyCommand::new(bin())
        .cwd(&b.parent)
        .isolated_lastcall(&b.home, &b.config_path, &b.state)
        .args(["tui"]);
    let cmd = if sample_rss { cmd.sample_rss() } else { cmd };
    cmd.spawn().expect("spawn lastcall tui in a pty")
}

fn s5_quit(pty: &mut PtyTui) {
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(LONG).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
}

/// Every root's ledger under the temp state dir: `(parked records, distinct parked trees)`
/// per root, read with `serde_json` as counts only.
fn s5_parked(state: &Path) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let Ok(parents) = std::fs::read_dir(state.join("roots")) else {
        return out;
    };
    for parent in parents.flatten() {
        let Ok(repos) = std::fs::read_dir(parent.path().join("repos")) else {
            continue;
        };
        for repo in repos.flatten() {
            let Ok(bytes) = std::fs::read(repo.path().join("ledger.json")) else {
                continue;
            };
            let Ok(ledger) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let Some(branches) = ledger.get("branches").and_then(|b| b.as_object()) else {
                out.push((0, 0));
                continue;
            };
            let trees: BTreeSet<&str> = branches
                .values()
                .filter_map(|r| r.get("seen_tree").and_then(|t| t.as_str()))
                .collect();
            out.push((branches.len(), trees.len()));
        }
    }
    out
}

/// S5: one repository and six linked worktrees under one parent, six writers, three
/// ramp / churn / quiet cycles under the default timings, then a relaunch over the same
/// state. The memory and CPU curve of the TUI under that load (Phase 17 Part A); read as
/// deltas within one run (`docs/dev/bench.md`, S5).
#[test]
#[ignore]
fn bench_s5_worktree_soak() {
    fresh_line();
    if cfg!(debug_assertions) {
        note("SKIP: S5 measures the release build only (run: just bench-soak)");
        return;
    }
    let run = Instant::now();
    let knobs = S5Knobs::from_env();
    s5_line(&format!(
        "--- S5 knobs: scale={} files={} lines={} stride={} worktrees={S5_WORKTREES} \
         branch_every={}",
        knobs.scale,
        knobs.files,
        knobs.lines,
        knobs.stride,
        knobs.branch_every()
    ));
    s5("stride", knobs.stride);
    let b = Bench::new("s5", None);
    // The tools' output files live in their own temp dir, beside the state dir, not in it.
    let tools_dir = TempDir::new("lc-bench-s5-tools");
    let tools = tools_dir.path().to_path_buf();

    // The fixture, outside every measured region.
    let built = Instant::now();
    let mut repo = b.repo("repo");
    let mut bytes = 0usize;
    for f in 0..knobs.files {
        let text = s5_text(f, knobs.lines, knobs.stride, None);
        bytes += text.len();
        repo.write(&s5_file(f), text);
    }
    repo.commit("the soak's tree").expect("commit");
    let mut writers: Vec<S5Writer> = (1..=S5_WORKTREES)
        .map(|i| {
            let dir = b.parent.join(format!("w{i:02}"));
            repo.git(&[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("agent/w{i:02}"),
                dir.to_str().expect("a utf-8 temp path"),
            ])
            .expect("worktree add");
            S5Writer {
                index: i,
                dir,
                seq: 0,
                writes: 0,
                churn_writes: 0,
                switches: 0,
                failures: 0,
            }
        })
        .collect();
    let roots = 1 + S5_WORKTREES;
    b.first_sight(roots);
    s5("fixture_mb", format!("{:.1}", bytes as f64 / 1_048_576.0));
    s5("fixture_s", format!("{:.1}", built.elapsed().as_secs_f64()));

    let t0 = Instant::now();
    let mut pty = s5_spawn(&b, false);
    pty.wait_for_text(&format!("nothing pending across {roots} repos"), LONG)
        .unwrap_or_else(|e| panic!("every root listed: {e}"));
    pty.wait_for(LONG, watching)
        .unwrap_or_else(|e| panic!("watching: {e}"));
    s5("roots", roots);
    s5(
        "worktree_badges",
        // The summary lists each root as `  name  branch  [badge]`; the nav column draws
        // the badge too but may cut it, so count worktrees by the summary's line.
        pty.screen(|s| {
            let (_, cols) = s.size();
            let rows: Vec<String> = s.rows(0, cols).collect();
            (1..=S5_WORKTREES)
                .filter(|i| {
                    let line = format!("w{i:02}  agent/w{i:02}  [worktree of repo]");
                    rows.iter().any(|r| r.contains(&line))
                })
                .count()
        }),
    );
    let mut sampler = S5Sampler::start(pty.pid().expect("pid"), t0, 0, "launch");

    let stop = AtomicBool::new(false);
    let churn = AtomicBool::new(false);
    for cycle in 1..=S5_CYCLES {
        stop.store(false, Ordering::Relaxed);
        churn.store(false, Ordering::Relaxed);
        let quiet = std::thread::scope(|s| {
            for w in writers.iter_mut() {
                let (repo, knobs, stop, churn) = (&repo, &knobs, &stop, &churn);
                s.spawn(move || s5_write_loop(repo, w, knobs, stop, churn));
            }
            let ramp = sampler.begin(cycle, "ramp");
            s5_idle(&pty, ramp.at + knobs.phase(S5_RAMP));
            s5_phase_end(&sampler, &pty, &ramp, &tools, true);

            churn.store(true, Ordering::Relaxed);
            let phase = sampler.begin(cycle, "churn");
            let until = phase.at + knobs.phase(S5_CHURN);
            let mut next = phase.at + S5_ACCEPT_EVERY;
            let mut started = 0u32;
            while Instant::now() < until {
                if Instant::now() >= next {
                    if s5_accept(&mut pty) {
                        started += 1;
                        sampler.event("accept");
                    } else {
                        sampler.event("refused");
                    }
                    next += S5_ACCEPT_EVERY;
                }
                s5_idle(&pty, until.min(next));
            }
            s5_phase_end(&sampler, &pty, &phase, &tools, true);
            s5(&format!("c{cycle}_churn_accepts_started"), started);
            churn.store(false, Ordering::Relaxed);

            // The quiet phase starts as the writers are told to stop; the scope's end
            // joins them before anything in it is measured.
            let quiet = sampler.begin(cycle, "quiet");
            stop.store(true, Ordering::Relaxed);
            quiet
        });
        s5_quiet(&sampler, &mut pty, &quiet, knobs.phase(S5_QUIET));
        s5_phase_end(&sampler, &pty, &quiet, &tools, false);
    }

    // The relaunch: writers on with nothing accepted, writers joined, quit, start again
    // over the same state. The launch's `scan_all` is where up to eight roots' batches stack.
    let relaunch_cycle = S5_CYCLES + 1;
    stop.store(false, Ordering::Relaxed);
    std::thread::scope(|s| {
        for w in writers.iter_mut() {
            let (repo, knobs, stop, churn) = (&repo, &knobs, &stop, &churn);
            s.spawn(move || s5_write_loop(repo, w, knobs, stop, churn));
        }
        let phase = sampler.begin(relaunch_cycle, "prequit");
        s5_idle(&pty, phase.at + knobs.phase(S5_RELAUNCH_WRITE));
        let end = sampler.end(&phase);
        let header = pty.screen(s5_header);
        s5_opt("prequit_end_rss_kb", end.end_kb);
        s5_opt("prequit_peak_rss_kb", end.peak_kb);
        s5_opt("prequit_end_files", header.map(|h| h.0));
        s5_opt("prequit_end_hunks", header.map(|h| h.1));
        stop.store(true, Ordering::Relaxed);
    });
    let mut failed_ticks = sampler.finish();
    drop(sampler);
    s5_quit(&mut pty);
    drop(pty);

    let spawned = Instant::now();
    let mut pty = s5_spawn(&b, true);
    let pid = pty.pid().expect("pid");
    let mut sampler = S5Sampler::start(pid, t0, relaunch_cycle, "relaunch");
    let phase = sampler.begin(relaunch_cycle, "relaunch");
    pty.wait_for(LONG, |s| s5_header(s).is_some())
        .unwrap_or_else(|e| panic!("relaunch first frame: {e}"));
    s5(
        "relaunch_first_frame_s",
        format!("{:.1}", spawned.elapsed().as_secs_f64()),
    );
    s5_idle(&pty, spawned + knobs.phase(S5_RELAUNCH_SAMPLE));
    let end = sampler.end(&phase);
    // The 10 ms sampler's maximum, read at the window's end before any tool runs: the
    // launch peak; the 1 s sampler's own maximum beside it.
    s5_opt("relaunch_peak_rss_kb", pty.peak_rss_kb());
    if pty.rss_sampler_stopped_early() {
        s5_line("--- S5 relaunch_peak_rss_kb is PARTIAL: the 10 ms sampler stopped early");
    }
    s5_opt("relaunch_peak_rss_kb_1hz", end.peak_kb);
    s5_opt("relaunch_end_rss_kb", end.end_kb);
    s5_opt(
        "relaunch_cpu_pct_avg",
        end.cpu_pct_avg.map(|c| format!("{c:.1}")),
    );
    s5_memory_lines("relaunch", &s5_memory(&tools, pid));
    let leaks = s5_tool(
        &tools,
        "leaks",
        &["--nostacks", "--noContent", &pid.to_string()],
    )
    .and_then(|t| parse_leaks(&t));
    s5_opt("leaks_count", leaks.map(|l| l.0));
    s5_opt("leaks_bytes", leaks.map(|l| l.1));
    failed_ticks += sampler.finish();
    drop(sampler);
    s5_quit(&mut pty);

    let parked = s5_parked(&b.state);
    s5(
        "parked_records_max",
        parked.iter().map(|p| p.0).max().unwrap_or(0),
    );
    s5(
        "parked_trees_distinct",
        parked.iter().map(|p| p.1).sum::<usize>(),
    );
    s5_line(&format!(
        "--- S5 ledgers (parked records, distinct parked trees) per root: {parked:?}"
    ));
    s5(
        "writer_switches",
        writers.iter().map(|w| w.switches).sum::<u64>(),
    );
    s5(
        "writer_failures",
        writers.iter().map(|w| w.failures).sum::<u64>(),
    );
    s5_line(&format!(
        "--- S5 writers: {}; sampler ticks without a number: {failed_ticks}",
        writers
            .iter()
            .map(|w| format!(
                "w{:02} {} writes {} switches",
                w.index, w.writes, w.switches
            ))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    s5("wall_s", run.elapsed().as_secs());
}

// ---- S5's readers: plain functions over the tools' text, tested below without a run ----

/// `ps -o time=` as seconds. macOS prints unbounded minutes (`3020:03.94`), Linux
/// `[DD-]HH:MM:SS`: both are fields split on `:` and summed from the right (seconds, then
/// minutes, then hours), the last one fractional, after an optional `DD-` day prefix.
fn parse_cpu_time(text: &str) -> Option<f64> {
    let text = text.trim();
    let (days, rest) = match text.split_once('-') {
        Some((d, rest)) => (d.parse::<u64>().ok()?, rest),
        None => (0, text),
    };
    let fields: Vec<&str> = rest.split(':').collect();
    if fields.len() > 3 {
        return None;
    }
    let mut total = 0.0;
    let mut unit = 1.0;
    for (i, field) in fields.iter().rev().enumerate() {
        let value = if i == 0 {
            field.parse::<f64>().ok()?
        } else {
            field.parse::<u64>().ok()? as f64
        };
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        total += value * unit;
        unit *= 60.0;
    }
    Some(total + days as f64 * 86_400.0)
}

/// `ps -o rss=,time= -p <pid>`: the resident set (KiB) and the cumulative CPU seconds.
fn parse_ps_sample(text: &str) -> Option<(u64, f64)> {
    let mut fields = text.split_whitespace();
    let rss = fields.next()?.parse().ok()?;
    let cpu = parse_cpu_time(fields.next()?)?;
    if fields.next().is_some() {
        return None;
    }
    Some((rss, cpu))
}

/// `56` `MB` → KiB. The macOS memory tools print `B`, `KB`, `MB` and `GB` (binary units),
/// sometimes with a fraction.
fn size_kb(number: &str, unit: &str) -> Option<u64> {
    let n: f64 = number.parse().ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    let kb = match unit {
        "B" => n / 1024.0,
        "KB" | "K" => n,
        "MB" | "M" => n * 1024.0,
        "GB" | "G" => n * 1024.0 * 1024.0,
        "TB" | "T" => n * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some(kb.round() as u64)
}

/// `footprint <pid>`: the `Footprint:` total from the banner, and the `Reclaimable` column
/// summed over the rows whose category starts with `MALLOC`, both in KiB. A row reads
/// `<dirty> <unit> <clean> <unit> <reclaimable> <unit> <regions> <category…>`.
fn parse_footprint(text: &str) -> Option<(u64, u64)> {
    let total = text.lines().find_map(|line| {
        let (_, after) = line.split_once("Footprint:")?;
        let mut tokens = after.split_whitespace();
        size_kb(tokens.next()?, tokens.next()?)
    })?;
    let reclaimable = text
        .lines()
        .filter_map(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            if tokens.len() < 8 || !tokens[7].starts_with("MALLOC") {
                return None;
            }
            size_kb(tokens[4], tokens[5])
        })
        .sum();
    Some((total, reclaimable))
}

/// `heap <pid>`: the malloc zones' in-use total, `All zones: <n> nodes (<bytes> bytes)`, in
/// KiB. The earlier `All zones: … malloced - Sizes:` line has no byte total and is skipped.
fn parse_heap_inuse_kb(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("All zones:")?;
        let (_, bytes) = rest.split_once('(')?;
        let bytes: u64 = bytes.strip_suffix(" bytes)")?.trim().parse().ok()?;
        Some(bytes.div_ceil(1024))
    })
}

/// `heap <pid>`'s banner: `Physical footprint:         56.2M`, taken while `heap` holds the
/// task suspended, so it pairs with [`parse_heap_inuse_kb`] at one instant. The unit is
/// glued to the number (`752K`, `56.2M`, `1.1G`); the `(peak)` line is not this one.
fn parse_heap_footprint_kb(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let value = line.trim().strip_prefix("Physical footprint:")?.trim();
        let split = value.find(|c: char| !c.is_ascii_digit() && c != '.')?;
        let (number, unit) = value.split_at(split);
        size_kb(number, unit.trim())
    })
}

/// `leaks <pid>`: `Process <pid>: <n> leaks for <bytes> total leaked bytes.`
fn parse_leaks(text: &str) -> Option<(u64, u64)> {
    text.lines().find_map(|line| {
        if !line.contains("total leaked bytes") {
            return None;
        }
        let (_, rest) = line.split_once(": ")?;
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        let count = tokens.first()?.parse().ok()?;
        let bytes = tokens.get(3)?.parse().ok()?;
        Some((count, bytes))
    })
}

/// The header's counts, `lastcall  <R> repos · <F> files · <H> hunks …`, as `(F, H)`; a
/// capped count (`1,203+ files`) reads as its number. `None` during the launch hold
/// (`<R> repos · checking status…`) or on any other first row.
fn parse_header_counts(row: &str) -> Option<(usize, usize)> {
    let rest = row.trim_start().strip_prefix("lastcall  ")?;
    let mut parts = rest.split(" · ");
    parts.next()?;
    let number = |part: &str, noun: &str| -> Option<usize> {
        let mut tokens = part.split_whitespace();
        let n = tokens.next()?.replace([',', '+'], "").parse().ok()?;
        tokens.next()?.starts_with(noun).then_some(n)
    };
    let files = number(parts.next()?, "file")?;
    let hunks = number(parts.next()?, "hunk")?;
    Some((files, hunks))
}

#[test]
fn bench_s5_ps_time_reads_macos_minutes_and_linux_days() {
    let close = |text: &str, want: f64| {
        let got = parse_cpu_time(text).unwrap_or_else(|| panic!("{text:?} parses"));
        assert!((got - want).abs() < 1e-6, "{text:?}: {got} != {want}");
    };
    // macOS: minutes are unbounded, seconds carry hundredths.
    close("0:00.01", 0.01);
    close("  0:12.50\n", 12.5);
    close("3020:03.94", 3020.0 * 60.0 + 3.94);
    // Linux: `[DD-]HH:MM:SS`.
    close("00:00:00", 0.0);
    close("01:02:03", 3723.0);
    close(
        "2-03:04:05",
        2.0 * 86_400.0 + 3.0 * 3600.0 + 4.0 * 60.0 + 5.0,
    );
    // A bare seconds field is still a number.
    close("7.25", 7.25);
    for bad in [
        "",
        "abc",
        "1:2:3:4",
        "1:xx.5",
        "-1",
        "x-01:02:03",
        "1:-2.0",
        "NaN",
    ] {
        assert_eq!(parse_cpu_time(bad), None, "{bad:?}");
    }
    assert_eq!(
        parse_ps_sample(" 15888   3020:14.25\n").map(|(r, _)| r),
        Some(15888)
    );
    let (_, cpu) = parse_ps_sample("15888 3020:14.25").expect("two columns");
    assert!((cpu - 181_214.25).abs() < 1e-6, "{cpu}");
    assert_eq!(parse_ps_sample(""), None, "a gone process prints nothing");
    assert_eq!(parse_ps_sample("15888"), None);
    assert_eq!(
        parse_ps_sample("RSS TIME\n1 0:00.01"),
        None,
        "a header is not a sample"
    );
}

#[test]
fn bench_s5_memory_tools_text_parses() {
    let footprint = "\
======================================================================
lastcall [5535]: 64-bit    Footprint: 1.5 GB (16384 bytes per page)
======================================================================

  Dirty      Clean  Reclaimable    Regions    Category
    ---        ---          ---        ---    ---
  55 MB        0 B       2 MB          6    MALLOC_MEDIUM
 273 KB        0 B          0 B          1    page table
 144 KB        0 B       16 KB         12    MALLOC metadata
  64 KB        0 B      512 B           2    MALLOC_SMALL
    0 B      16 KB          0 B         45    __TEXT
    ---        ---          ---        ---    ---
  56 MB      48 KB          3 MB        344    TOTAL

Auxiliary data:
    phys_footprint: 56 MB
";
    assert_eq!(
        parse_footprint(footprint),
        Some((1_572_864, 2048 + 16 + 1)),
        "the banner's total, MALLOC rows' reclaimable only (TOTAL is not a MALLOC row)"
    );
    assert_eq!(parse_footprint("no such process"), None);
    assert_eq!(size_kb("56", "MB"), Some(57_344));
    assert_eq!(size_kb("776", "B"), Some(1));
    assert_eq!(size_kb("3", "parsecs"), None);

    let heap = "\
Physical footprint:         56.2M
----

Process 5535: 1 zone

All zones: 678 nodes malloced - Sizes: 128KB[500] 5KB[1] 32[144]

-----------------------------------------------------------------------
All zones: 678 nodes (65550288 bytes)
";
    assert_eq!(parse_heap_inuse_kb(heap), Some(64_014));
    assert_eq!(parse_heap_inuse_kb("heap: no process"), None);
    assert_eq!(
        parse_heap_footprint_kb(heap),
        Some(57_549),
        "56.2M, glued unit"
    );
    let glued = |v: &str| {
        parse_heap_footprint_kb(&format!(
            "Physical footprint (peak):  9.9G\nPhysical footprint:         {v}\n"
        ))
    };
    assert_eq!(glued("752K"), Some(752), "the (peak) line is skipped");
    assert_eq!(glued("55.7M"), Some(57_037));
    assert_eq!(glued("1.1G"), Some(1_153_434));
    assert_eq!(glued("0B"), Some(0));
    assert_eq!(glued("12"), None, "no unit");
    assert_eq!(glued("4Q"), None);
    assert_eq!(parse_heap_footprint_kb("heap: no process"), None);

    let leaks = "\
leaks Report Version: 4.0
Process 5535: 678 nodes malloced for 64014 KB
Process 5535: 501 leaks for 65541120 total leaked bytes.
";
    assert_eq!(parse_leaks(leaks), Some((501, 65_541_120)));
    assert_eq!(
        parse_leaks("Process 9: 0 leaks for 0 total leaked bytes."),
        Some((0, 0))
    );
    assert_eq!(
        parse_leaks("Process 9: 678 nodes malloced for 64014 KB"),
        None
    );
}

#[test]
fn bench_s5_stride_edits_one_line_in_stride() {
    let base = s5_text(7, S5_LINES, S5_STRIDE, None);
    assert_eq!(
        base.len(),
        S5_LINES * 38,
        "38 bytes a line: 34.2 MB at 300 files"
    );
    let edited = s5_text(7, S5_LINES, S5_STRIDE, Some((3, 151)));
    let changed: Vec<usize> = base
        .lines()
        .zip(edited.lines())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(j, _)| j)
        .collect();
    assert_eq!(changed.len(), 20, "one line in 150 of 3,000");
    assert_eq!(changed[0], 1, "seq 151 edits lines 1, 151, 301 …");
    assert!(changed.windows(2).all(|p| p[1] - p[0] == S5_STRIDE));
}

#[test]
fn bench_s5_header_counts_read_the_first_row() {
    assert_eq!(
        parse_header_counts(
            "lastcall  7 repos · 1,203 files · 45,000 hunks  [Accept All]  watching W"
        ),
        Some((1_203, 45_000))
    );
    assert_eq!(
        parse_header_counts("lastcall  1 repo · 1 file · 1 hunk"),
        Some((1, 1))
    );
    assert_eq!(
        parse_header_counts("lastcall  3 repos · 10,000+ files · 9 hunks"),
        Some((10_000, 9))
    );
    assert_eq!(
        parse_header_counts("lastcall  7 repos · checking status…"),
        None
    );
    assert_eq!(parse_header_counts("watching W"), None);
}
