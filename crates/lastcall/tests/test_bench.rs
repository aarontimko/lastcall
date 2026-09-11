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

use std::io::Write;
use std::path::{Path, PathBuf};
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
