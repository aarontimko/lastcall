//! Phase 3 PTY tier (kickoff deliverable 10): the **built binary**
//! (`env!("CARGO_BIN_EXE_lastcall")`) inside a real pseudo-terminal through
//! `lastcall_testkit::pty_tui`, over the same three-root fixture as the snapshots and the
//! golden, with `tui --poll 1` so every scene passes on the polling backstop alone (the
//! development Mac's fseventsd delivers nothing; where events arrive the scenes are faster,
//! not different). Assertions poll the `vt100` screen; nothing sleeps a fixed amount.
//!
//! Every child gets its own temp parent dir, state dir and `HOME` (`PtyCommand::
//! isolated_lastcall`): the sponsor's `~/.local/state/lastcall` and `~/.config` are never
//! touched. The scenes run one at a time (`SERIAL`) so the edit-to-screen measurement is
//! not perturbed by a sibling scene's fixture build. Timings and skips are written with
//! `stderr().write_all`, which libtest does not swallow.
//!
//! Phase 4 (kickoff deliverable 9(b)) adds the scripted agent loop through the same
//! binary — `pty_accept_loop_and_restart` (accept hunk, file, everything; `ledger.json`
//! read back; `q`; the agent commits; a **second process** on the same state dir shows
//! nothing pending) — and the CAS refusal on screen, `pty_accept_refused_when_file_moves`.
//!
//! `probe_tui_screen` (ignored) is `just probe-tui-screen`: the same flow against the
//! release binary, printing the final screen and the exit code.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lastcall_testkit::fixture_parent;
use lastcall_testkit::fixture_repo::FixtureRepo;
use lastcall_testkit::mock_herdr::MockHerdr;
use lastcall_testkit::pty_tui::{PtyCommand, PtyTui, col_of, vt100};
use lastcall_testkit::tmp::TempDir;

/// `commands/tui.rs::NOT_A_TERMINAL` (the binary crate's private module; kept in sync by
/// `pty_non_tty_stdout_exits_2_without_drawing`).
const NOT_A_TERMINAL: &str = "lastcall: not a terminal; try `lastcall status`";
/// `render::TOO_SMALL`.
const TOO_SMALL: &str = "too small: 40×10 min";
/// `commands/tui.rs::DISCOVERING` (deliverable 7), kept in sync by `wait_first_piles`.
const DISCOVERING: &str = "lastcall: discovering roots under ";

/// The engine's debounce plus one second: the §8 Phase 3 gate's live-update budget.
const LIVE_UPDATE_BUDGET: Duration = Duration::from_millis(1750);
/// `q` / Ctrl-C to process exit (kickoff step 5).
const QUIT_BUDGET: Duration = Duration::from_secs(2);
/// The first pile on an idle host arrives in well under this; beyond it the timing
/// assertion is skipped with a visible reason (overloaded host), the rest still runs.
const OVERLOADED: Duration = Duration::from_secs(10);
/// Hard bound on any single wait.
const LONG: Duration = Duration::from_secs(30);

const ALT_SCREEN_ON: &[u8] = b"\x1b[?1049h";
const ALT_SCREEN_OFF: &[u8] = b"\x1b[?1049l";
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";
const MOUSE_ON: &[u8] = b"\x1b[?1000h";
/// crossterm's `DisableMouseCapture`, in the order it writes them.
const MOUSE_OFF: [&[u8]; 5] = [
    b"\x1b[?1006l",
    b"\x1b[?1015l",
    b"\x1b[?1003l",
    b"\x1b[?1002l",
    b"\x1b[?1000l",
];

static SERIAL: Mutex<()> = Mutex::new(());

fn note(line: &str) {
    let mut err = std::io::stderr();
    let _ = err.write_all(line.as_bytes());
    let _ = err.write_all(b"\n");
    let _ = err.flush();
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Skip CSI escape sequences (`ESC [ … final`) starting at `at`.
fn skip_escapes(hay: &[u8], mut at: usize) -> usize {
    while hay[at..].starts_with(b"\x1b[") {
        match hay[at + 2..].iter().position(|b| (0x40..=0x7e).contains(b)) {
            Some(i) => at += 2 + i + 1,
            None => break,
        }
    }
    at
}

/// Where `words` first appear in order with nothing but escape sequences between them: one
/// rendered line. A frame's text is not contiguous in the raw bytes — ratatui skips cells
/// equal to the previous buffer, so every blank of a fresh line becomes a cursor move — but
/// no other text may sit between two words of the same line, and a CSI parameter (`3;1H`)
/// cannot pass for a word.
fn find_words(hay: &[u8], words: &[&str]) -> Option<usize> {
    let (head, rest) = words.split_first()?;
    let mut start = 0;
    'candidates: loop {
        let first = start + find(&hay[start..], head.as_bytes())?;
        let mut at = first + head.len();
        for word in rest {
            at = skip_escapes(hay, at);
            if hay[at..].starts_with(word.as_bytes()) {
                at += word.len();
            } else {
                start = first + 1;
                continue 'candidates;
            }
        }
        return Some(first);
    }
}

#[test]
fn find_words_needs_one_line_not_a_csi_parameter() {
    let one_line = b"scanning\x1b[12C3\x1b[1C\x1b[0mroots\xe2\x80\xa6";
    assert_eq!(find_words(one_line, &["scanning", "3", "roots…"]), Some(0));
    // blanks are cursor moves; the `3` of the `3;1H` cursor move is not a word
    let other_line = b"scanning\x1b[1C4\x1b[1Croots\xe2\x80\xa6\x1b[3;1Hnothing\x1b[1Cpending\x1b[1Cacross\x1b[1C4\x1b[1Croots";
    assert_eq!(find_words(other_line, &["scanning", "3", "roots…"]), None);
    assert_eq!(
        find_words(other_line, &["scanning", "4", "roots…"]),
        Some(0)
    );
    assert!(find_words(other_line, &["pending", "across", "4"]).is_some_and(|i| i > 0));
    assert_eq!(
        find_words(other_line, &["scanning", "roots…"]),
        None,
        "no word may be skipped"
    );
}

struct Fixture {
    _w: TempDir,
    _state: TempDir,
    parent: PathBuf,
    state: PathBuf,
    home: PathBuf,
    config: PathBuf,
}

impl Fixture {
    /// `fixture_parent::build` under `<tmp>/W/` (the header reads `watching W`) with the
    /// config the binary reads through `LASTCALL_CONFIG`.
    fn build() -> Fixture {
        let w = TempDir::new("lc-pty-w");
        let state = TempDir::new("lc-pty-state");
        let parent = w.join("W");
        let built = fixture_parent::build(&parent, state.path()).expect("fixture builds");
        let config = state.join("config.toml");
        fixture_parent::write_config(&config, &parent).expect("config written");
        Fixture {
            parent,
            state: state.path().to_path_buf(),
            home: built.home,
            config,
            _w: w,
            _state: state,
        }
    }

    /// [`Fixture::build`] plus **this scene's own** fourth root (Phase 6 deliverable
    /// 1(b)): `W/alpha/_drafts/reply.md` at `baseline`, first-sighted in-process before
    /// the binary ever runs — the child's own first sight would otherwise take the
    /// *edited* file as the baseline and show nothing pending — and a `config.toml`
    /// naming both draft dirs, so the child discovers four roots. The shared fixture and
    /// its three-root assertion are untouched.
    fn with_draft_root(baseline: &str) -> (Fixture, PathBuf) {
        let w = TempDir::new("lc-pty-w");
        let state = TempDir::new("lc-pty-state");
        let parent = w.join("W");
        let built = fixture_parent::build(&parent, state.path()).expect("fixture builds");
        let drafts = fixture_parent::add_draft_root(&built, state.path(), baseline)
            .expect("the scene's fourth root");
        let config = state.join("config.toml");
        fixture_parent::write_draft_config(&config, &parent).expect("config written");
        let fx = Fixture {
            parent,
            state: state.path().to_path_buf(),
            home: built.home,
            config,
            _w: w,
            _state: state,
        };
        (fx, drafts)
    }

    fn command(&self, bin: &Path) -> PtyCommand {
        PtyCommand::new(bin).cwd(&self.parent).isolated_lastcall(
            &self.home,
            &self.config,
            &self.state,
        )
    }

    /// `lastcall tui --poll 1` in a 100×30 PTY; `None` only when this host has no PTY
    /// (written as a visible SKIP).
    fn spawn_tui(&self, bin: &Path) -> Option<PtyTui> {
        self.spawn_tui_env(bin, &[])
    }

    /// [`Fixture::spawn_tui`] with extra environment for the child. Only the Phase 8 editor
    /// scenes use it, to point `$EDITOR` at their own probe script: `isolated_lastcall`
    /// removes `$VISUAL` and `$EDITOR` from every child, so a scene that says nothing here
    /// cannot reach an editor at all.
    fn spawn_tui_env(&self, bin: &Path, env: &[(&str, OsString)]) -> Option<PtyTui> {
        let mut cmd = self.command(bin).args(["tui", "--poll", "1"]);
        for (key, value) in env {
            cmd = cmd.env(key, value);
        }
        match cmd.spawn() {
            Ok(p) => Some(p),
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                note(&format!("SKIP: this host cannot open a pty: {e}"));
                None
            }
            Err(e) => panic!("spawn lastcall tui: {e}"),
        }
    }

    /// The headless spawn (stdout a pipe) with the same isolation.
    fn headless(&self, bin: &Path, args: &[&str]) -> std::process::Output {
        Command::new(bin)
            .args(args)
            .current_dir(&self.parent)
            .env("HOME", &self.home)
            .env("LASTCALL_CONFIG", &self.config)
            .env("LASTCALL_STATE_DIR", &self.state)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .output()
            .expect("run lastcall")
    }

    fn append(&self, rel: &str, line: &str) {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(self.parent.join(rel))
            .expect("open fixture file");
        f.write_all(line.as_bytes()).expect("append");
        f.flush().expect("flush");
    }

    /// The fixture "agent"'s handle on one of the repos (`alpha`, `beta`): writes and
    /// commits with the fixture's fixed identity, never through the engine.
    fn repo(&self, name: &str) -> FixtureRepo {
        FixtureRepo::open_in(TempDir::adopt(&self.parent), name)
    }

    /// `ledger.json` of the root at `<parent>/<name>`, found under
    /// `<state>/roots/<parent-id>/repos/<root-id>/` by its `root` field (the canonical
    /// path) — read from disk, exactly what the next process will load.
    fn ledger(&self, name: &str) -> serde_json::Value {
        self.ledger_in(name).1
    }

    /// The root's state dir (`ledger.json`, `store/`, `index`) and its ledger.
    fn ledger_in(&self, name: &str) -> (PathBuf, serde_json::Value) {
        let root = std::fs::canonicalize(self.parent.join(name)).expect("root exists");
        let root = root.to_string_lossy().into_owned();
        let roots = self.state.join("roots");
        let mut found = Vec::new();
        for parent in std::fs::read_dir(&roots)
            .expect("state/roots exists")
            .flatten()
        {
            let repos = parent.path().join("repos");
            let Ok(entries) = std::fs::read_dir(&repos) else {
                continue;
            };
            for repo in entries.flatten() {
                let path = repo.path().join("ledger.json");
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let value: serde_json::Value = serde_json::from_str(&text)
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                if value["root"].as_str() == Some(root.as_str()) {
                    found.push((repo.path(), value));
                }
            }
        }
        assert_eq!(found.len(), 1, "exactly one ledger for {root}: {found:?}");
        found.pop().unwrap()
    }
}

/// The status bar reads `<text> · <age>` for exactly `text` (an `accepted f1` status must
/// not pass for `accepted f1 · 1 hunk left`).
fn status_is(s: &vt100::Screen, text: &str) -> bool {
    let (_, cols) = s.size();
    s.rows(0, cols).last().is_some_and(|r| {
        r.trim_end()
            .strip_prefix(&format!("{text} · "))
            .is_some_and(|age| !age.is_empty() && !age.contains(' '))
    })
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lastcall"))
}

/// The nav lists alpha's rows: the first piles arrived.
fn rows_listed(s: &vt100::Screen) -> bool {
    let text = s.contents();
    text.contains("M f1") && text.contains("M f2") && text.contains("M n2.md")
}

/// Wait for the first piles; returns how long they took. Also proves the first frame was
/// the launch hold: `discovered 3 repos, checking status…` precedes the first file row in
/// the raw transcript — and the empty state (`nothing pending across 3 repos`) never does,
/// since every root here has rows (the Gate 8 sponsor run's ruling: no repo is listed until
/// every root has reported). Design pass D3 (ruling R5): the pane is the hold's only home,
/// so the harness anchors on the pane text; `run.rs` no longer writes `scanning N roots…`
/// to the status line, which this also pins. Returns only once the watch is live (the
/// `watching …` status is on the bottom row), so the scene's first key never races the
/// notice that would otherwise cover what it waits for.
/// The status row reads `watching <parent> (3 roots)`: the FSEvents watch is installed and
/// its gap-closing rescans are done, so from here a file change is found by the live watch
/// and its debounce rather than by a startup rescan. The temp path is long, so only the
/// head of the line is asserted.
fn wait_watching(pty: &mut PtyTui) {
    pty.wait_for(LONG, |s| {
        let (_, cols) = s.size();
        s.rows(0, cols)
            .last()
            .is_some_and(|r| r.starts_with("watching "))
    })
    .unwrap_or_else(|e| panic!("the watch is live: {e}"));
}

fn wait_first_piles(pty: &mut PtyTui) -> Duration {
    let took = pty
        .wait_for(LONG, rows_listed)
        .unwrap_or_else(|e| panic!("first piles: {e}"));
    let raw = pty.raw();
    let hold = find_words(&raw, &["discovered", "3", "repos,", "checking", "status…"])
        .expect("first frame: the launch hold");
    let first_row = find(&raw, b"f1").expect("a file row");
    assert!(
        hold < first_row,
        "the launch hold ({hold}) precedes the first row ({first_row})"
    );
    assert!(
        find_words(&raw, &["scanning", "3", "roots…"]).is_none(),
        "D3: the status line does not carry a second sentence about the same wait"
    );
    assert!(
        find_words(&raw, &["nothing", "pending", "across", "3", "repos"])
            .is_none_or(|i| i > first_row),
        "the empty state never shows before the rows"
    );
    let alt_on = find(&raw, ALT_SCREEN_ON).expect("alternate screen on");
    assert!(find(&raw, MOUSE_ON).is_some(), "mouse capture is on");
    // Phase 6 deliverable 7: discovery can take seconds over a large tree, and a terminal
    // that prints nothing reads as a hang. `commands/tui.rs` writes one stderr line before
    // `Engine::open`, so it lands in the transcript **before** the alternate screen is
    // taken, and the first frame - drawn after - replaces it.
    let discovering = find(&raw, DISCOVERING.as_bytes()).unwrap_or_else(|| {
        panic!(
            "the discovering line: {:?}",
            String::from_utf8_lossy(&raw[..raw.len().min(400)])
        )
    });
    assert!(
        discovering < alt_on,
        "the discovering line ({discovering}) precedes alternate-screen-on ({alt_on})"
    );
    assert!(
        alt_on < hold,
        "the first frame ({hold}) comes after it ({alt_on})"
    );
    assert!(pty.screen(|s| s.alternate_screen()));
    assert!(pty.screen(|s| s.mouse_protocol_mode() != vt100::MouseProtocolMode::None));
    // The scene starts once the watch is live, not once the rows are up. The engine says
    // `watching <parent> (N roots)` only after the watcher is installed and its gap-closing
    // rescans are done, which on a CI runner is seconds after the first piles (here it is
    // milliseconds). Without this a scene that pressed a key in that gap saw the notice
    // land on the status row moments later — over the hint line it was waiting for, or
    // over the verdict a key had just posted (PR #9's first CI run: `t show empty` and
    // `focused demo in herdr` both lost to `watching …`). From here the status row holds
    // the notice for `STATUS_TTL`, the same start every scene has on a fast machine.
    wait_watching(pty);
    took
}

/// Poll the raw transcript until it contains `needle`; returns how long that took. The
/// screen is no use here: the bytes wanted are a terminal *query*, which vt100 consumes
/// without drawing anything.
fn wait_raw(pty: &mut PtyTui, needle: &[u8], timeout: Duration) -> Duration {
    let start = Instant::now();
    loop {
        if find(&pty.raw(), needle).is_some() {
            return start.elapsed();
        }
        assert!(
            start.elapsed() < timeout,
            "{:?} not written within {timeout:?}; transcript:\n{:?}",
            String::from_utf8_lossy(needle),
            String::from_utf8_lossy(&pty.raw())
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// After an exit: the transcript's tail leaves the terminal sane and carries no tracing.
fn assert_clean_exit(pty: &PtyTui, since: usize) {
    assert!(pty.wait_eof(Duration::from_secs(2)), "reader saw EOF");
    let raw = pty.raw();
    let tail = &raw[since..];
    let mut last = 0;
    for seq in MOUSE_OFF {
        let at = find(tail, seq).unwrap_or_else(|| {
            panic!(
                "mouse-off {:?} missing from the tail: {:?}",
                seq,
                String::from_utf8_lossy(tail)
            )
        });
        assert!(at >= last, "mouse-off sequences in crossterm's order");
        last = at;
    }
    assert!(
        find(tail, ALT_SCREEN_OFF).is_some(),
        "alternate screen left"
    );
    assert!(find(tail, CURSOR_SHOW).is_some(), "cursor shown again");
    assert!(
        pty.screen(|s| !s.alternate_screen()),
        "vt100 agrees: main screen"
    );
    assert!(
        pty.screen(|s| s.mouse_protocol_mode() == vt100::MouseProtocolMode::None),
        "vt100 agrees: mouse reporting off"
    );
    for level in ["INFO", "WARN", "ERROR", "DEBUG", "TRACE"] {
        assert!(
            find(&raw, level.as_bytes()).is_none(),
            "no tracing output in the transcript ({level})"
        );
    }
}

/// The screen rows containing a hunk header, as `(row, text)`.
fn hunk_headers(pty: &PtyTui) -> Vec<(u16, String)> {
    pty.rows()
        .into_iter()
        .enumerate()
        .filter(|(_, r)| r.contains("@@ -"))
        .map(|(i, r)| (i as u16, r))
        .collect()
}

/// Whether the hunk header in `row` (its `@@` cell) is drawn inverted.
fn header_inverted(pty: &PtyTui, row: u16) -> bool {
    let text = &pty.rows()[row as usize];
    let col = col_of(text, "@@ -").expect("a hunk header row");
    pty.inverse_at(row, col)
}

/// The `@@ -a,b +c,d @@` text of a screen row holding a hunk header.
fn header_text(row: &str) -> String {
    let start = row.find("@@ -").expect("a hunk header row");
    let rest = &row[start..];
    let end = rest[4..].find("@@").expect("the closing @@") + 4 + 2;
    rest[..end].to_owned()
}

// ---- scenes ----------------------------------------------------------------------------

/// Kickoff steps 1–5: first frame, the live-update measurement (two tries, min ≤ 1.75 s),
/// `n` moves the inverted hunk header, a real mouse click selects beta, `q` exits 0 within
/// 2 s and leaves the terminal sane.
#[test]
fn pty_live_update_hunk_nav_click_and_quit() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };

    // (1) first frame, then the piles.
    let first = wait_first_piles(&mut pty);
    note(&format!("PTY first piles on screen after {first:.3?}"));
    assert!(
        first <= LONG,
        "the nav shows alpha within the bound (took {first:?})"
    );

    // (2) two appends to alpha/f1; the clock starts after each write returns.
    // Baseline f1 is a1..a10; the fixture already changed line 1, so an appended line
    // makes a second hunk (needed by step 3) and the row reads `M f1  +2 −1`, then `+3 −1`.
    fx.append("alpha/f1", "appended 1\n");
    let try1 = pty
        .wait_for_text("M f1  +2 −1", OVERLOADED)
        .unwrap_or_else(|e| panic!("first edit never reached the screen: {e}"));
    fx.append("alpha/f1", "appended 2\n");
    let try2 = pty
        .wait_for_text("M f1  +3 −1", OVERLOADED)
        .unwrap_or_else(|e| panic!("second edit never reached the screen: {e}"));
    let best = try1.min(try2);
    note(&format!(
        "PTY edit-to-screen: try1={try1:.3?} try2={try2:.3?} min={best:.3?} (budget {LIVE_UPDATE_BUDGET:?}, tui --poll 1)"
    ));
    if first > OVERLOADED {
        note(&format!(
            "SKIP: live-update budget not asserted, the first piles took {first:.3?} (> {OVERLOADED:?}: overloaded host)"
        ));
    } else {
        assert!(
            best <= LIVE_UPDATE_BUDGET,
            "edit-to-screen min of two tries {best:?} exceeds {LIVE_UPDATE_BUDGET:?} (try1 {try1:?}, try2 {try2:?})"
        );
    }
    // The header's totals are live too: 3 repos, 6 files, 9 hunks (f1 now has two, and
    // alpha's `src/parse.rs` carries the fixture's three separated hunks).
    pty.wait_for_text("3 repos · 6 files · 9 hunks", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("header totals: {e}"));

    // (3) select f1 (↓ to alpha, ↓ to f1), open it, then `n`: the second hunk header is
    // the inverted one.
    pty.send(b"jj\r").expect("keys");
    pty.wait_for_text("f1  M  +3 −1", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("main view header for f1: {e}"));
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().matches("@@ -").count() == 2
    })
    .unwrap_or_else(|e| panic!("two hunk headers: {e}"));
    let headers = hunk_headers(&pty);
    assert_eq!(headers.len(), 2, "{headers:?}");
    let (row1, h1) = headers[0].clone();
    let (row2, h2) = headers[1].clone();
    assert!(header_inverted(&pty, row1), "hunk 1 is current before `n`");
    assert!(!header_inverted(&pty, row2), "hunk 2 is not current yet");
    pty.send(b"n").expect("n");
    let (h1_header, h2_header) = (header_text(&h1), header_text(&h2));
    assert_ne!(h1_header, h2_header);
    pty.wait_for(Duration::from_secs(5), |s| {
        // The cursor scrolls hunk 2's header to the top of the diff, inverted.
        let (_, cols) = s.size();
        s.rows(0, cols).enumerate().any(|(i, r)| {
            r.contains(&h2_header)
                && col_of(&r, "@@ -")
                    .and_then(|c| s.cell(i as u16, c))
                    .is_some_and(vt100::Cell::inverse)
        })
    })
    .unwrap_or_else(|e| panic!("after `n` hunk 2's header is inverted: {e}"));
    for (row, text) in hunk_headers(&pty) {
        if text.contains(&h1_header) {
            assert!(!header_inverted(&pty, row), "hunk 1 is no longer current");
        }
    }

    // (4) a real mouse click on beta's repo row selects it: the main view header becomes
    // beta's; a second click on its `u1` row opens a beta path.
    let beta_row = pty
        .find_row(|r| r.starts_with("│beta"))
        .expect("beta's repo row is on screen");
    pty.click(3, beta_row).expect("click");
    pty.wait_for_text("beta  main · 2 files", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("clicking beta selects it: {e}"));
    let u1_row = pty
        .find_row(|r| r.starts_with("│  A u1"))
        .expect("beta's u1 row is on screen");
    pty.click(5, u1_row).expect("click");
    pty.wait_for_text("u1  A  +1 −0", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("clicking u1 opens it: {e}"));

    // (5) `q`: exit 0 within the budget, terminal restored.
    let since = pty.raw().len();
    let started = Instant::now();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    let quit = started.elapsed();
    note(&format!(
        "PTY q-to-exit {quit:.3?} (budget {QUIT_BUDGET:?})"
    ));
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// Ctrl-C under raw mode is a key event bound to `quit`: exit 0, terminal restored.
#[test]
fn pty_ctrl_c_exits_cleanly() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    let since = pty.raw().len();
    let started = Instant::now();
    pty.send(b"\x03").expect("ctrl-c");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after ctrl-c");
    let quit = started.elapsed();
    note(&format!(
        "PTY ctrl-c-to-exit {quit:.3?} (budget {QUIT_BUDGET:?})"
    ));
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// A resize below 40×10 draws only the too-small line; growing back redraws the app.
#[test]
fn pty_resize_below_minimum_shows_too_small_then_recovers() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    pty.resize(30, 8).expect("resize");
    pty.wait_for(Duration::from_secs(5), |s| s.contents().trim() == TOO_SMALL)
        .unwrap_or_else(|e| panic!("too-small frame: {e}"));
    pty.resize(100, 30).expect("resize");
    pty.wait_for(Duration::from_secs(5), |s| {
        rows_listed(s) && s.contents().contains("watching W")
    })
    .unwrap_or_else(|e| panic!("the app redraws at 100×30: {e}"));
    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    assert_eq!(pty.wait_exit(QUIT_BUDGET).expect("exits").exit_code(), 0);
    assert_clean_exit(&pty, since);
}

/// Stdout a pipe: exit 2 with the one message on stderr, nothing on stdout — for both
/// spellings, bare `lastcall` and `lastcall tui`.
#[test]
fn pty_non_tty_stdout_exits_2_without_drawing() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    for args in [&[][..], &["tui"][..], &["tui", "--poll", "1"][..]] {
        let out = fx.headless(&bin(), args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr).trim_end(),
            NOT_A_TERMINAL,
            "{args:?}"
        );
        assert!(
            out.stdout.is_empty(),
            "{args:?}: nothing drawn into the pipe"
        );
    }
}

/// A `[keys]` table that does not parse: exit 2 with the message, before any escape
/// sequence is written — the terminal is never taken. `lastcall config` reports it too.
#[test]
fn pty_bad_keys_config_exits_2_before_taking_the_terminal() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let cases: [(&str, &str); 3] = [
        (
            "[keys]\nfrobnicate = \"x\"\n",
            "unknown action `frobnicate`",
        ),
        (
            "[keys]\nquit = \"hyper-q\"\n",
            "quit: bad key spec \"hyper-q\"",
        ),
        (
            "[keys]\nhelp = \"q\"\n",
            "\"q\" is bound to both `help` and `quit`",
        ),
    ];
    let base = fixture_parent::config_toml(&fx.parent);
    for (table, message) in cases {
        std::fs::write(&fx.config, format!("{base}{table}")).expect("config written");

        let Some(mut pty) = fx.spawn_tui(&bin()) else {
            return;
        };
        let status = pty.wait_exit(Duration::from_secs(10)).expect("exits");
        assert_eq!(status.exit_code(), 2, "{table}: {status:?}");
        assert!(pty.wait_eof(Duration::from_secs(2)));
        let raw = pty.raw();
        let text = String::from_utf8_lossy(&raw);
        assert!(
            text.contains(message),
            "{table}: message on the pty:\n{text}"
        );
        assert!(
            find(&raw, ALT_SCREEN_ON).is_none() && find(&raw, MOUSE_ON).is_none(),
            "{table}: no escape sequence before the exit:\n{text}"
        );
        assert!(
            find(&raw, b"\x1b[").is_none(),
            "{table}: no CSI sequence at all:\n{text}"
        );

        let out = fx.headless(&bin(), &["config"]);
        assert_eq!(out.status.code(), Some(2), "{table}: config exits 2");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(message),
            "{table}: config names it: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// `just probe-tui-screen`: the release binary (`LASTCALL_PROBE_BIN`, else the test
/// binary) over the fixture, one scripted edit, the diff opened, the screen printed.
/// `cargo test -p lastcall --test test_e2e_tui_pty probe_tui_screen -- --ignored --nocapture`
#[test]
#[ignore]
fn probe_tui_screen() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let bin = std::env::var_os("LASTCALL_PROBE_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(bin);
    let fx = Fixture::build();
    note(&format!(
        "--- {} tui --poll 1  (parent {}, state {})",
        bin.display(),
        fx.parent.display(),
        fx.state.display()
    ));
    let Some(mut pty) = fx.spawn_tui(&bin) else {
        return;
    };
    let first = wait_first_piles(&mut pty);
    note(&format!(
        "--- first piles after {first:.3?}; appending a line to alpha/f1"
    ));
    fx.append("alpha/f1", "appended by probe-tui-screen\n");
    let took = pty
        .wait_for_text("M f1  +2 −1", OVERLOADED)
        .unwrap_or_else(|e| panic!("edit never reached the screen: {e}"));
    note(&format!(
        "--- counts changed on screen after {took:.3?}; opening f1"
    ));
    pty.send(b"jj\r").expect("keys");
    pty.wait_for_text("f1  M  +2 −1", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("diff view: {e}"));
    note("--- screen (100×30) ---");
    for row in pty.rows() {
        note(&row);
    }
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits");
    note(&format!("--- exit={}", status.exit_code()));
}

/// `f1` with line 1 and line 10 changed against the seen `a1..a10`: two separated hunks.
const F1_TWO_HUNKS: &str = "A1\na2\na3\na4\na5\na6\na7\na8\na9\nA10\n";
const F2_EDIT: &str = "b\nagent edit\nmore\n";
const F2_EDIT_AGAIN: &str = "b\nagent edit\nmore\nagain\n";
const F3_EDIT: &str = "c changed\n";
/// Added files so that after the hunk and file accepts `ctrl-a` still covers twelve files
/// (> `CONFIRM_ABOVE`), which drives the confirm modal through the real terminal.
const ADDED: [&str; 6] = ["g01", "g02", "g03", "g04", "g05", "g06"];

/// Kickoff deliverable 9(b): the whole reviewer loop through the real binary. The fixture
/// "agent" writes and commits in `alpha` before the reviewer looks; `a` (hunk), `A`
/// (file) and `ctrl-a` + `y` (everything, across three roots, above the confirm threshold)
/// shrink the nav to `nothing pending`; the ledgers on disk carry the fold; `q` exits 0;
/// the agent commits the rest; a **second process** on the same state dir shows the empty
/// state once its scans are done, and one more edit shows exactly that delta.
#[test]
fn pty_accept_loop_and_restart() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let alpha = fx.repo("alpha");

    // The agent: f1 (two hunks), f2, f3 committed, six added files.
    alpha.write("f1", F1_TWO_HUNKS);
    alpha.write("f2", F2_EDIT);
    alpha.write("f3", F3_EDIT);
    alpha.git(&["add", "f3"]).expect("git add f3");
    alpha
        .git(&["commit", "-q", "-m", "agent: f3"])
        .expect("git commit f3");
    let f3_commit = alpha.head().expect("HEAD");
    for name in ADDED {
        alpha.write(name, format!("{name}\n"));
    }
    let before = fx.ledger("alpha");
    assert_eq!(before["overrides"], serde_json::json!({}), "{before}");

    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    let first = wait_first_piles(&mut pty);
    pty.wait_for(LONG, |s| {
        let t = s.contents();
        t.contains("M f3") && t.contains("A g06") && t.contains("3 repos · 13 files")
    })
    .unwrap_or_else(|e| panic!("all thirteen rows: {e}"));
    note(&format!(
        "PTY accept loop: 13 files on screen after {first:.3?}"
    ));

    // (1) open f1: two hunks; `a` accepts the first, the row shrinks to the second.
    pty.send(b"jj\r").expect("keys");
    pty.wait_for(Duration::from_secs(5), |s| {
        let t = s.contents();
        t.contains("f1  M  +2 −2") && t.matches("@@ -").count() == 2
    })
    .unwrap_or_else(|e| panic!("f1 open with two hunks: {e}"));
    let t = Instant::now();
    pty.send(b"a").expect("a");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "accepted f1 · 1 hunk left") && s.contents().contains("M f1  +1 −1")
    })
    .unwrap_or_else(|e| panic!("accept hunk: {e}"));
    note(&format!(
        "PTY accept hunk: status + row after {:.3?}",
        t.elapsed()
    ));

    // (2) `A` accepts the file: f1 leaves the nav, the selection lands on f2.
    let t = Instant::now();
    pty.send(b"A").expect("A");
    pty.wait_for(OVERLOADED, |s| {
        let text = s.contents();
        status_is(s, "accepted f1") && !text.contains("M f1") && text.contains("f2  M  +2 −0")
    })
    .unwrap_or_else(|e| panic!("accept file: {e}"));
    note(&format!(
        "PTY accept file: f1 gone after {:.3?}",
        t.elapsed()
    ));

    // (3) `ctrl-a`: twelve files across three roots is above the confirm threshold; the
    // modal counts u1 as the one grouped upstream row; `y` folds all three ledgers.
    pty.send(b"\x01").expect("ctrl-a");
    pty.wait_for(Duration::from_secs(5), |s| {
        let text = s.contents();
        text.contains("Accept all 12 files across 3 repos?")
            && text.contains("1 grouped upstream · 0 collapsed")
    })
    .unwrap_or_else(|e| panic!("confirm modal: {e}"));
    let t = Instant::now();
    pty.send(b"y").expect("y");
    // Amendment v1.9: the three repos stay on the nav with nothing under them, and the
    // cursor is on alpha's own name row, so the pane names the repo instead of counting
    // roots. The header keeps counting every repo it is watching.
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "accepted 12 files in 3 repos")
            && s.contents().contains("nothing pending in alpha")
    })
    .unwrap_or_else(|e| panic!("accept all: {e}"));
    note(&format!(
        "PTY accept all: nothing pending after {:.3?}",
        t.elapsed()
    ));
    pty.wait_for_text("3 repos · 0 files · 0 hunks", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("empty header: {e}"));

    // The fold persisted: what the next process will load.
    let (alpha_state, after) = fx.ledger_in("alpha");
    assert_eq!(after["overrides"], serde_json::json!({}), "{after}");
    assert_ne!(
        after["seen_tree"], before["seen_tree"],
        "alpha's seen tree moved"
    );
    assert_eq!(
        after["seen_at"]["head_commit"].as_str(),
        Some(f3_commit.as_str()),
        "seen_at is the agent's commit the fold happened on: {after}"
    );
    // The seen tree itself, listed from the root's store: one entry per path of alpha's
    // working tree as accepted and nothing else — f1 and f2 at the blobs the agent left
    // (hashed here from the same files: f1 is the whole edit, hunk then file), f3 as the
    // agent's commit has it, the six added files, and the fixture's `src/parse.rs`.
    let store = alpha_state.join("store");
    let seen_tree = after["seen_tree"].as_str().expect("seen_tree is an oid");
    let listing = alpha
        .git_at(
            &fx.parent,
            &[
                "--git-dir",
                store.to_str().expect("utf-8 store path"),
                "ls-tree",
                "-r",
                seen_tree,
            ],
        )
        .expect("ls-tree of the seen tree in the store");
    let entries: BTreeMap<&str, &str> = listing
        .lines()
        .map(|line| {
            let (meta, path) = line.split_once('\t').expect("mode type oid\\tpath");
            (path, meta.split_whitespace().nth(2).expect("oid"))
        })
        .collect();
    let mut expected = vec!["f1", "f2", "f3"];
    expected.extend(ADDED);
    expected.push(fixture_parent::PARSE_RS);
    assert_eq!(
        entries.keys().copied().collect::<Vec<_>>(),
        expected,
        "the seen tree is alpha's working tree, path for path:\n{listing}"
    );
    let blob = |rel: &str| {
        alpha
            .git(&["hash-object", rel])
            .expect("hash-object")
            .trim()
            .to_owned()
    };
    assert_eq!(
        entries["f1"],
        blob("f1"),
        "f1 seen as the agent's whole edit"
    );
    assert_eq!(entries["f2"], blob("f2"), "f2 seen as the agent's edit");
    assert_eq!(
        entries["f3"],
        alpha
            .git(&["rev-parse", "HEAD:f3"])
            .expect("HEAD:f3")
            .trim(),
        "f3 seen as the agent committed it"
    );
    for name in ["beta", "notes"] {
        let ledger = fx.ledger(name);
        assert_eq!(ledger["overrides"], serde_json::json!({}), "{ledger}");
        assert!(ledger["seen_tree"].is_string(), "{name} folded: {ledger}");
    }

    // (4) `q`, then the agent commits the rest behind the reviewer's back.
    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
    drop(pty);
    alpha
        .git(&["commit", "-q", "-a", "-m", "agent: the rest"])
        .expect("git commit -a");
    assert_ne!(alpha.head().expect("HEAD"), f3_commit);

    // (5) a second process on the same state dir: three empty repo rows, after the launch
    // hold (`discovered 3 repos, checking status…` in the pane; the `watching <parent>
    // (3 roots)` status lands on the bottom row once the post-install rescans are done —
    // the temp path is long, so only its head fits).
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    let t = Instant::now();
    wait_watching(&mut pty);
    assert!(
        find_words(
            &pty.raw(),
            &["discovered", "3", "repos,", "checking", "status…"]
        )
        .is_some(),
        "the relaunch held while it scanned"
    );
    note(&format!("PTY relaunch: scanned after {:.3?}", t.elapsed()));
    let text = pty.screen_text();
    // Amendment v1.9: every repo stays on the nav, so this is three name-and-branch rows;
    // and every one of them is empty, so the pane is the empty state rather than a prompt
    // to select a file that is not there (verifier (a) F5).
    assert!(text.contains("nothing pending across 3 repos"), "{text}");
    assert!(text.contains("3 repos · 0 files · 0 hunks"), "{text}");
    let raw = pty.raw();
    for row in ["M f1", "M f2", "M f3", "A g01", "A u1", "M n2.md"] {
        assert!(find(&raw, row.as_bytes()).is_none(), "{row} never drawn");
    }
    assert_eq!(
        fx.ledger("alpha"),
        after,
        "the agent's commit did not touch the ledger"
    );

    // (6) one more edit shows exactly that delta.
    alpha.write("f2", F2_EDIT_AGAIN);
    let took = pty
        .wait_for_text("M f2  +1 −0", OVERLOADED)
        .unwrap_or_else(|e| panic!("the re-edit: {e}"));
    // Amendment v1.9: the header counts every repo on the nav, and the other two are
    // still there with nothing pending.
    pty.wait_for_text("3 repos · 1 file · 1 hunk", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("header after the re-edit: {e}"));
    note(&format!("PTY relaunch edit-to-screen {took:.3?}"));
    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// §6.7 (Amendment v1.9), deliverable 2, through the real binary: `t` names its own
/// inverse on the hint line; accepting a repo's last file lands the cursor on the repo's
/// **own name row** — the repo is still on the nav, now a name-and-branch row with nothing
/// under it, and the pane reads `nothing pending in notes`; `t` then hides it and `t`
/// brings it back.
///
/// Two things shape the order. The bottom row is the **status** line while a status is
/// live, and launch sets one (`watching <parent> (3 roots)`), so the hint-line half waits
/// out `app::STATUS_TTL` (30s) once — which also puts it before the accept, while every
/// repo still has rows and `t` therefore hides nothing but the label. And the scene widens
/// the terminal for it: the toggle is near the middle of deliverable 4's drop order, so the
/// scene reads it at a width where the whole line fits rather than depending on what the
/// 100-column default happens to keep.
#[test]
fn pty_accept_last_file_lands_on_the_repo_row_then_t_hides_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);

    // (1) the label follows the state, and with nothing empty `t` hides nothing. The
    // 40s bound is `STATUS_TTL` plus room: the launch status has to age out before the
    // hint line is what the bottom row shows.
    pty.resize(240, 30).expect("resize");
    pty.wait_for(Duration::from_secs(40), |s| {
        s.contents().contains("t hide empty")
    })
    .unwrap_or_else(|e| panic!("the toggle is on the hint line: {e}"));
    pty.send(b"t").expect("t");
    pty.wait_for(Duration::from_secs(5), |s| {
        let t = s.contents();
        t.contains("t show empty") && t.contains("M n2.md")
    })
    .unwrap_or_else(|e| panic!("`t` flips the label: {e}\n{}", pty.screen_text()));
    pty.send(b"t").expect("t back");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("t hide empty")
    })
    .unwrap_or_else(|e| panic!("`t` is its own inverse: {e}\n{}", pty.screen_text()));

    // (2) notes has exactly one pending file, so `A` on it is the repo's last file.
    select_until(&mut pty, "n2.md  M ");
    pty.send(b"A").expect("A");
    pty.wait_for(OVERLOADED, |s| {
        let t = s.contents();
        status_is(s, "accepted n2.md") && t.contains("nothing pending in notes")
    })
    .unwrap_or_else(|e| {
        panic!(
            "the repo row, not the next repo: {e}\n{}",
            pty.screen_text()
        )
    });

    let text = pty.screen_text();
    assert!(!text.contains("M n2.md"), "the file row is gone: {text}");
    assert!(text.contains("3 repos · 5 files"), "{text}");
    let row = pty
        .find_row(|r| r.starts_with("\u{2502}notes"))
        .unwrap_or_else(|| panic!("notes is still on the nav:\n{text}"));
    assert!(
        pty.inverse_at(row, 1),
        "the repo row carries the cursor:\n{text}"
    );

    // (3) and now `t` has something to hide.
    pty.send(b"t").expect("t");
    pty.wait_for(Duration::from_secs(5), |s| {
        let t = s.contents();
        !t.contains("notes") && t.contains("2 repos · 5 files")
    })
    .unwrap_or_else(|e| panic!("`t` hides the empty repo: {e}\n{}", pty.screen_text()));

    pty.send(b"t").expect("t again");
    pty.wait_for(Duration::from_secs(5), |s| {
        let t = s.contents();
        t.contains("notes") && t.contains("3 repos · 5 files")
    })
    .unwrap_or_else(|e| panic!("`t` brings it back: {e}\n{}", pty.screen_text()));

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// The draft file's 20-line baseline (F1's shape): two later edits six lines apart are two
/// hunks at `CONTEXT` 3, not one merged hunk.
fn draft_baseline() -> String {
    (1..=20).map(|i| format!("line {i}\n")).collect()
}

/// The agent's edit: lines 2 and 18 rewritten — two hunks.
fn draft_edited() -> String {
    (1..=20)
        .map(|i| match i {
            2 | 18 => format!("line {i} edited by the agent\n"),
            _ => format!("line {i}\n"),
        })
        .collect()
}

/// Move the nav selection to `header`'s row. The number of steps depends on `alpha`'s own
/// pending rows, which these scenes deliberately do not pin (the `.gitignore` one of them
/// writes is one); the diff pane follows the selection without `⏎`, so this needs the nav
/// focus only.
///
/// It walks to the **top** of the nav first and only then downward: since Amendment v1.9
/// the cursor never wraps inside a repo, so an accept can leave it *below* the row a scene
/// wants next (`pty_editor_save_pends_nothing` blesses `src/parse.rs` and lands on `f2`,
/// with `f1` above it).
fn select_until(pty: &mut PtyTui, header: &str) {
    // The walk needs the **nav** focused: in the diff pane `k`/`j` scroll the pane and the
    // selection never moves. `Esc` (`back`) puts the focus there from either pane and does
    // nothing else once it is there, so it is safe to send unconditionally. The wait after
    // it is not cosmetic — a bare `\x1b` is an ambiguous prefix, and a key that lands in
    // the same read makes it `Alt-<key>`, which swallows the Esc.
    pty.send(b"\x1b").expect("esc to the nav");
    if pty
        .wait_for(Duration::from_millis(400), |s| {
            s.contents().contains(header)
        })
        .is_ok()
    {
        return;
    }
    for _ in 0..24 {
        pty.send(b"k").expect("k");
    }
    for _ in 0..24 {
        if pty
            .wait_for(Duration::from_millis(400), |s| {
                s.contents().contains(header)
            })
            .is_ok()
        {
            return;
        }
        pty.send(b"j").expect("j");
    }
    panic!("never selected {header}:\n{}", pty.screen_text());
}

/// Phase 6 gate item 1, through the real binary: a **gitignored draft file inside a git
/// repo** goes pending, is reviewed one hunk at a time, and the fold survives a restart.
/// The engine half is `scenario_f1_gitignored_draft_dir_inside_a_repo`; this is the same
/// sequence a reviewer performs — `⏎`, `a`, `q`, relaunch, `A` — over a **fourth root the
/// scene owns**, so `fixture_parent::build`'s three-root assertion, the status golden and
/// all 59 snapshots stay exactly as they are.
#[test]
fn pty_draft_root_hunk_accept_and_restart() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (fx, drafts) = Fixture::with_draft_root(&draft_baseline());
    let reply = drafts.join("reply.md");
    // The agent edits the draft after first sight: two hunks pending.
    std::fs::write(&reply, draft_edited()).expect("the agent's edit");

    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    pty.wait_for(LONG, |s| s.contents().contains("M reply.md  +2 −2"))
        .unwrap_or_else(|e| panic!("the draft row: {e}"));
    let raw = pty.raw();
    assert!(
        find_words(&raw, &["discovered", "4", "repos,", "checking", "status…"]).is_some(),
        "the child discovered the scene's fourth root"
    );
    let text = pty.screen_text();
    assert!(
        text.contains("_drafts"),
        "the draft root is in the nav:\n{text}"
    );
    assert!(
        text.contains("draft · 1 file"),
        "the row sits under the `draft` label:\n{text}"
    );

    // `⏎` opens the row: two hunks, the agent's two edited lines.
    select_until(&mut pty, "reply.md  M  +2 −2");
    pty.send(b"\r").expect("open");
    pty.wait_for(Duration::from_secs(5), |s| {
        let t = s.contents();
        t.matches("@@ -").count() == 2
            && t.contains("+line 2 edited by the agent")
            && t.contains("+line 18 edited by the agent")
    })
    .unwrap_or_else(|e| panic!("two hunks open: {e}"));

    // `a` accepts the first hunk; the second stays pending.
    let t = Instant::now();
    pty.send(b"a").expect("a");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "accepted reply.md · 1 hunk left")
            && s.contents().contains("M reply.md  +1 −1")
    })
    .unwrap_or_else(|e| panic!("accept the first hunk: {e}"));
    note(&format!(
        "PTY draft root: hunk accepted after {:.3?}",
        t.elapsed()
    ));
    let text = pty.screen_text();
    assert!(
        !text.contains("+line 2 edited by the agent"),
        "the accepted hunk is gone:\n{text}"
    );
    assert!(
        text.contains("+line 18 edited by the agent"),
        "the untouched hunk remains:\n{text}"
    );

    // `q`, then a second process over the same state dir: the same one hunk.
    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
    drop(pty);

    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    pty.wait_for(LONG, |s| s.contents().contains("M reply.md  +1 −1"))
        .unwrap_or_else(|e| panic!("the relaunch shows the remaining hunk: {e}"));
    select_until(&mut pty, "reply.md  M  +1 −1");
    pty.wait_for(Duration::from_secs(5), |s| {
        let t = s.contents();
        t.matches("@@ -").count() == 1 && t.contains("+line 18 edited by the agent")
    })
    .unwrap_or_else(|e| panic!("one hunk after the restart: {e}"));

    // `A` takes the file whole: the draft root empties.
    pty.send(b"A").expect("A");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "accepted reply.md") && !s.contents().contains("M reply.md")
    })
    .unwrap_or_else(|e| panic!("accept the file: {e}"));
    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// Kickoff deliverable 9(b): the CAS refusal through the real terminal. The diff is open,
/// the agent rewrites the file, `A` is pressed before the watcher's debounce has rescanned:
/// the status says so and the row stays; once the screen shows the new counts, `A` accepts.
#[test]
fn pty_accept_refused_when_file_moves() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    // The race under test is against the 750 ms debounce of a *live* watch, so the watch
    // has to be live first. The `watching …` notice arrives only after the FSEvents install
    // and its gap-closing rescan of every root; on a loaded runner that landed after `A`,
    // replacing the refusal on the status row and rescanning the append (CI macos-latest
    // 2026-09-05). Before the watch is live the append would be found by that rescan, not
    // by the debounce.
    wait_watching(&mut pty);
    pty.send(b"jj\r").expect("keys");
    pty.wait_for_text("f1  M  +1 −1", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("f1 open: {e}"));

    fx.append("alpha/f1", "moved after render\n");
    let t = Instant::now();
    pty.send(b"A").expect("A");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "f1: changed since rendered; not accepted")
    })
    .unwrap_or_else(|e| panic!("refusal status: {e}"));
    note(&format!(
        "PTY accept refused: status after {:.3?}",
        t.elapsed()
    ));
    assert!(pty.screen_text().contains("M f1"), "the row remains");
    assert_eq!(fx.ledger("alpha")["overrides"], serde_json::json!({}));

    pty.wait_for_text("M f1  +2 −1", OVERLOADED)
        .unwrap_or_else(|e| panic!("the rescan shows the new counts: {e}"));
    let t = Instant::now();
    pty.send(b"A").expect("A");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "accepted f1") && !s.contents().contains("M f1")
    })
    .unwrap_or_else(|e| panic!("accept after the rescan: {e}"));
    note(&format!(
        "PTY accept after refusal: f1 gone after {:.3?}",
        t.elapsed()
    ));
    assert!(fx.ledger("alpha")["overrides"]["f1"].is_object());

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

// --- Phase 5: herdr through the real terminal ------------------------------------------

/// The recorded two-pane snapshot with every pane and agent moved into `root`, so §6.6
/// association has somewhere real to land: `w1:p1` is the agent (`demo`), `w1:p2` the bare
/// shell beside it.
fn herdr_snapshot(root: &Path) -> serde_json::Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../lastcall-testkit/fixtures/herdr/snapshot_two_panes.json"
    );
    let text = std::fs::read_to_string(path).expect("the recorded snapshot fixture");
    let mut v: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    let cwd = serde_json::json!(root.to_string_lossy());
    for key in ["panes", "agents"] {
        for entry in v["snapshot"][key].as_array_mut().expect("an array") {
            entry["cwd"] = cwd.clone();
            entry["foreground_cwd"] = cwd.clone();
        }
    }
    v
}

/// One `pane.agent_status_changed` line for the mock to push down the live status stream.
fn status_line(pane_id: &str, status: &str) -> String {
    serde_json::json!({
        "event": "pane.agent_status_changed",
        "data": {"agent": "demo", "agent_status": status, "pane_id": pane_id,
                 "workspace_id": "w1"}
    })
    .to_string()
}

/// Gate 5's PTY scene. The built binary talks to the socket mock over `HERDR_SOCKET_PATH`:
/// the header names the version, alpha carries the working dot, the agent finishes where
/// nobody is looking (`done`) and the row grows a bright flag, `d` dims it (bold → not, the
/// one attribute vt100 keeps for us), `g` sends `agent.focus` with the **public pane id**
/// and the status says so, and `q` still exits clean with a live link.
#[test]
fn pty_herdr_flag_ack_jump() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let alpha = std::fs::canonicalize(fx.parent.join("alpha")).expect("alpha exists");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime for the mock");
    let sock = fx.state.join("herdr.sock");
    let mock = rt.block_on(async {
        MockHerdr::builder()
            .snapshot(herdr_snapshot(&alpha))
            .canned("agent.focus", serde_json::json!({"type": "ok"}))
            .canned(
                "notification.show",
                serde_json::json!({"type": "notification_shown", "shown": true, "reason": ""}),
            )
            .serve(&sock)
            .await
            .expect("bind the mock socket")
    });
    let control = mock.control();

    let Ok(mut pty) = fx
        .command(&bin())
        .args(["tui", "--poll", "1"])
        .env("HERDR_SOCKET_PATH", &sock)
        .spawn()
    else {
        note("SKIP: this host cannot open a pty");
        return;
    };
    wait_first_piles(&mut pty);
    let took = pty
        .wait_for_text("herdr 0.8.2", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("the header names the link: {e}"));
    note(&format!("PTY herdr: badge after {took:.3?}"));

    // The snapshot says `working`: a dot, and alpha is listed because it has rows anyway.
    let took = pty
        .wait_for(Duration::from_secs(5), |s| s.contents().contains("● alpha"))
        .unwrap_or_else(|e| panic!("the working dot: {e}"));
    note(&format!("PTY herdr: working dot after {took:.3?}"));

    // The agent finishes in a pane nobody is watching. Push it only once the client's
    // per-pane subscription is up, or the line would go nowhere.
    let subscribed = Instant::now();
    while control.status_streams_open("w1:p1") == 0 {
        assert!(
            subscribed.elapsed() < Duration::from_secs(10),
            "the client never subscribed to w1:p1: {:?}",
            control.methods()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let flipped = Instant::now();
    control.push_status("w1:p1", status_line("w1:p1", "done"));
    let took = pty
        .wait_for(Duration::from_secs(10), |s| {
            s.contents().contains("⚑ alpha")
        })
        .unwrap_or_else(|e| panic!("the flag: {e}"));
    note(&format!(
        "PTY herdr: done to flag in {took:.3?} (pushed {:.3?} ago)",
        flipped.elapsed()
    ));

    // Select alpha by clicking its **name** (a click on the dot would ack, which is the
    // parity path the unit tier owns) and read the flag cell's weight.
    let row = pty
        .find_row(|r| r.contains("⚑ alpha"))
        .expect("the flagged row");
    let text = pty.rows()[row as usize].clone();
    let name_col = col_of(&text, "alpha").expect("the name's column");
    let flag_col = col_of(&text, "⚑").expect("the flag's column");
    pty.click(name_col, row).expect("click the name");
    // The selected row is drawn inverted (the hint line is not visible here: the startup
    // `watching <dir>` status still owns the bottom row).
    pty.wait_for(Duration::from_secs(5), |s| {
        s.cell(row, name_col).is_some_and(vt100::Cell::inverse)
    })
    .unwrap_or_else(|e| panic!("the click selects alpha: {e}"));
    assert!(
        pty.screen(|s| s.cell(row, flag_col).is_some_and(vt100::Cell::bold)),
        "an unacked flag is bright"
    );

    let t = Instant::now();
    pty.send(b"d").expect("d");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.cell(row, flag_col).is_some_and(|c| !c.bold())
    })
    .unwrap_or_else(|e| panic!("the ack dims the flag: {e}"));
    note(&format!(
        "PTY herdr: d dims the flag after {:.3?}",
        t.elapsed()
    ));
    assert!(
        pty.screen_text().contains("⚑ alpha"),
        "still flagged, only dimmer — herdr still says done"
    );

    // `g` sends `agent.focus {"target": "w1:p1"}` on a one-shot connection.
    let t = Instant::now();
    pty.send(b"g").expect("g");
    pty.wait_for(Duration::from_secs(5), |s| {
        status_is(s, "focused demo in herdr")
    })
    .unwrap_or_else(|e| panic!("the jump verdict: {e}"));
    note(&format!("PTY herdr: g answered after {:.3?}", t.elapsed()));
    let focus: Vec<serde_json::Value> = control
        .requests()
        .into_iter()
        .filter(|r| r.method == "agent.focus")
        .map(|r| r.params)
        .collect();
    assert_eq!(
        focus,
        vec![serde_json::json!({"target": "w1:p1"})],
        "the public pane id, once, and never a display name"
    );

    let since = pty.raw().len();
    let t = Instant::now();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    note(&format!(
        "PTY herdr: quit with a live link in {:.3?}",
        t.elapsed()
    ));
    assert_clean_exit(&pty, since);
    rt.block_on(mock.shutdown());
}

/// One `worktree_created` line in herdr's lifecycle envelope (the same shape the D7
/// integration scene pushes).
fn worktree_created_line(path: &Path, branch: &str) -> String {
    serde_json::json!({
        "event": "worktree_created",
        "data": {
            "workspace": {"workspace_id": "w1", "label": "alpha"},
            "worktree": {
                "path": path.to_string_lossy(),
                "branch": branch,
                "is_linked_worktree": true
            }
        }
    })
    .to_string()
}

/// Review (b) F3: the link opens on its own task, so a socket that accepts and never
/// answers cannot hold the loop. `HERDR_SOCKET_PATH` is authoritative and unprobed, so the
/// whole 5 s request timeout of the protocol guard is spent against a stalled mock — and a
/// `q` pressed in that window still exits inside the quit budget.
#[test]
fn pty_herdr_a_stalled_socket_does_not_hold_the_keys() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime for the mock");
    let sock = fx.state.join("herdr.sock");
    let mock = rt.block_on(async {
        MockHerdr::builder()
            .stall()
            .serve(&sock)
            .await
            .expect("bind the mock socket")
    });

    let Ok(mut pty) = fx
        .command(&bin())
        .args(["tui", "--poll", "1"])
        .env("HERDR_SOCKET_PATH", &sock)
        .spawn()
    else {
        note("SKIP: this host cannot open a pty");
        return;
    };
    // The frame drawn *before* the link is opened; the guard is hanging from here on.
    pty.wait_for_text("checking status…", LONG)
        .unwrap_or_else(|e| panic!("the first frame: {e}"));

    let since = pty.raw().len();
    let t = Instant::now();
    pty.send(b"q").expect("q");
    let status = pty
        .wait_exit(QUIT_BUDGET)
        .expect("q during the connect still exits inside the quit budget");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    note(&format!(
        "PTY herdr: quit mid-connect in {:.3?} (the guard's timeout is 5 s)",
        t.elapsed()
    ));
    assert_clean_exit(&pty, since);
    rt.block_on(mock.shutdown());
}

/// Review (b) F4: deliverable 7 **through the loop**. The engine watches roots, not the
/// parent, and `--poll 300` parks the discovery backstop five minutes out, so a checkout
/// made after startup can reach the nav only through the loop's `worktree_due` arm — the
/// debounce it holds, and the `request_rescan` it then asks for.
#[test]
fn pty_herdr_worktree_created_reaches_the_nav_through_the_loop() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let alpha = std::fs::canonicalize(fx.parent.join("alpha")).expect("alpha exists");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime for the mock");
    let sock = fx.state.join("herdr.sock");
    let mock = rt.block_on(async {
        MockHerdr::builder()
            .snapshot(herdr_snapshot(&alpha))
            .serve(&sock)
            .await
            .expect("bind the mock socket")
    });
    let control = mock.control();

    let Ok(mut pty) = fx
        .command(&bin())
        .args(["tui", "--poll", "300"])
        .env("HERDR_SOCKET_PATH", &sock)
        .spawn()
    else {
        note("SKIP: this host cannot open a pty");
        return;
    };
    wait_first_piles(&mut pty);
    pty.wait_for_text("herdr 0.8.2", LONG)
        .unwrap_or_else(|e| panic!("the header names the link: {e}"));
    // The lifecycle subscription has to be up or the line goes nowhere.
    let subscribed = Instant::now();
    while control.lifecycle_streams_open() == 0 {
        assert!(
            subscribed.elapsed() < LONG,
            "the client never subscribed to the lifecycle stream: {:?}",
            control.methods()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // The user runs `git worktree add` in a herdr pane: a real linked checkout, with
    // something pending in it, under the same parent the TUI is watching.
    let repo = fx.repo("alpha");
    let checkout = fx.parent.join("alpha-wt");
    repo.git(&[
        "worktree",
        "add",
        "-b",
        "wt",
        &checkout.to_string_lossy(),
        "HEAD",
    ])
    .expect("git worktree add");
    std::fs::write(
        checkout.join("f1"),
        "worktree edit
",
    )
    .expect("edit in the new checkout");
    assert!(
        !pty.screen_text().contains("alpha-wt"),
        "nothing has told the loop about the checkout yet"
    );

    let t = Instant::now();
    control.push_lifecycle(worktree_created_line(&checkout, "wt"));
    let took = pty
        .wait_for_text("alpha-wt", LONG)
        .unwrap_or_else(|e| panic!("the new checkout never reached the nav: {e}"));
    note(&format!(
        "PTY herdr: worktree_created to a listed root in {took:.3?} \
         (discovery backstop parked at 300 s)"
    ));
    assert!(
        t.elapsed() < Duration::from_secs(60),
        "the trigger, not the backstop"
    );

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
    rt.block_on(mock.shutdown());
}

// --- Phase 7: restore and flag through the real terminal -------------------------------

/// `f1` as the fixture first sighted it: the bytes a restore must put back, byte for byte.
const F1_BASELINE: &str = "a1\na2\na3\na4\na5\na6\na7\na8\na9\na10\n";

/// The golden the standalone flag scene writes, with the clock and the root normalised.
const FLAG_EXPORT_PTY_GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/flag_export_pty.md"
);

/// `<state>/exports/<root>/<date>.md` — the one file the TUI writes. Found by listing
/// rather than by naming the date, so the scene does not race the clock over midnight.
fn export_file(state: &Path, root: &str) -> PathBuf {
    let dir = state.join("exports").join(root);
    let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .collect();
    found.sort();
    assert_eq!(
        found.len(),
        1,
        "one export file in {}: {found:?}",
        dir.display()
    );
    found.pop().expect("one file")
}

/// The export with everything a clock or a temp dir decides replaced: the `created_at`
/// field becomes `<T>` and the root field `<R>`, so the golden is about the *shape* of the
/// message the agent receives, which is what the human reads.
fn normalise_export(text: &str) -> String {
    // `split`, not `lines`: the trailing blank line is the separator the append writes, and
    // the golden is the place that pins it.
    text.split('\n')
        .map(|line| {
            let Some(rest) = line.strip_prefix("lastcall flag · ") else {
                return line.to_owned();
            };
            let mut fields: Vec<&str> = rest.split(" · ").collect();
            if let Some(first) = fields.first_mut() {
                *first = "<R>";
            }
            if let Some(last) = fields.last_mut() {
                *last = "<T>";
            }
            format!("lastcall flag · {}", fields.join(" · "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `u` then `U` on the same file: one hunk goes back, then the rest, and the file on disk
/// is the baseline byte for byte.
///
/// The point of the scene is the **bytes**, not the screen: `restore` is the one gesture
/// that writes into the reviewer's working tree, and a diff that renders as clean is not
/// the same claim as a file that is identical to what the agent started from.
#[test]
fn pty_restore_hunk_then_file_bytes_match_baseline() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let alpha = fx.repo("alpha");
    // Line 1 and line 10 rewritten: two hunks at CONTEXT 3, one to restore and one to leave.
    alpha.write("f1", F1_TWO_HUNKS);

    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    pty.send(b"jj\r").expect("keys");
    pty.wait_for(Duration::from_secs(5), |s| {
        let t = s.contents();
        t.contains("f1  M  +2 −2") && t.matches("@@ -").count() == 2
    })
    .unwrap_or_else(|e| panic!("f1 open with two hunks: {e}"));
    assert!(
        pty.screen_text().contains("[u restore]"),
        "the hunk control is on screen:\n{}",
        pty.screen_text()
    );

    // (1) `u` on the first hunk: no modal — a content hunk restore never asks.
    let t = Instant::now();
    pty.send(b"u").expect("u");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "restored f1 hunk 1") && s.contents().contains("M f1  +1 −1")
    })
    .unwrap_or_else(|e| panic!("restore hunk: {e}"));
    note(&format!(
        "PTY restore hunk: status + row after {:.3?}",
        t.elapsed()
    ));
    let on_disk = std::fs::read(fx.parent.join("alpha/f1")).expect("read f1");
    assert_eq!(
        String::from_utf8_lossy(&on_disk),
        "a1\na2\na3\na4\na5\na6\na7\na8\na9\nA10\n",
        "the first hunk went back and the second stayed"
    );

    // (2) `U` on the file: this one asks, and the modal says which file.
    pty.send(b"U").expect("U");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("Restore f1 · 1 hunk?")
    })
    .unwrap_or_else(|e| panic!("the restore confirm: {e}"));
    let t = Instant::now();
    pty.send(b"y").expect("y");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "restored f1") && !s.contents().contains("M f1")
    })
    .unwrap_or_else(|e| panic!("restore file: {e}"));
    note(&format!(
        "PTY restore file: f1 gone after {:.3?}",
        t.elapsed()
    ));
    let on_disk = std::fs::read(fx.parent.join("alpha/f1")).expect("read f1");
    assert_eq!(
        String::from_utf8_lossy(&on_disk),
        F1_BASELINE,
        "the working tree is the baseline, byte for byte"
    );
    // A restore is not an accept: nothing was folded into the ledger.
    assert_eq!(fx.ledger("alpha")["overrides"], serde_json::json!({}));

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// The file moves between the frame and the keystroke: the restore is refused by name, the
/// working tree is untouched, and the rescan that follows makes the next `u` land.
#[test]
fn pty_restore_refused_when_the_file_moved() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    pty.send(b"jj\r").expect("keys");
    pty.wait_for_text("f1  M  +1 −1", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("f1 open: {e}"));

    fx.append("alpha/f1", "moved after render\n");
    let before = std::fs::read(fx.parent.join("alpha/f1")).expect("read f1");
    let t = Instant::now();
    pty.send(b"u").expect("u");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "f1: changed since rendered; not restored")
    })
    .unwrap_or_else(|e| panic!("refusal status: {e}"));
    note(&format!(
        "PTY restore refused: status after {:.3?}",
        t.elapsed()
    ));
    assert_eq!(
        std::fs::read(fx.parent.join("alpha/f1")).expect("read f1"),
        before,
        "a refused restore writes nothing at all"
    );
    assert!(pty.screen_text().contains("M f1"), "the row remains");

    // The rescan re-renders the row; the same key now restores what is on screen.
    pty.wait_for_text("M f1  +2 −1", OVERLOADED)
        .unwrap_or_else(|e| panic!("the rescan shows the new counts: {e}"));
    let t = Instant::now();
    pty.send(b"u").expect("u");
    pty.wait_for(OVERLOADED, |s| status_is(s, "restored f1 hunk 1"))
        .unwrap_or_else(|e| panic!("restore after the rescan: {e}"));
    note(&format!(
        "PTY restore after refusal: status after {:.3?}",
        t.elapsed()
    ));

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// A deletion row: `u` is the whole file (there is no hunk to pick), it asks with the
/// `(deleted)` wording, and `y` puts the file back on disk.
#[test]
fn pty_restore_deletion_recreates_the_file() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let f3 = fx.parent.join("alpha/f3");
    std::fs::remove_file(&f3).expect("the agent deletes f3");

    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    pty.wait_for_text("D f3", LONG)
        .unwrap_or_else(|e| panic!("the deletion row: {e}"));
    select_until(&mut pty, "f3  D");

    // `u`, not `U`: on a deletion row the hunk key is the file key, because a deletion has
    // no hunk worth picking.
    pty.send(b"u").expect("u");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("Restore f3? (deleted)")
    })
    .unwrap_or_else(|e| panic!("the deletion confirm: {e}"));
    let t = Instant::now();
    pty.send(b"y").expect("y");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "restored f3") && !s.contents().contains("D f3")
    })
    .unwrap_or_else(|e| panic!("restore deletion: {e}"));
    note(&format!(
        "PTY restore deletion: f3 back after {:.3?}",
        t.elapsed()
    ));
    assert_eq!(
        std::fs::read_to_string(&f3).expect("f3 exists again"),
        "c\n",
        "the file is back at its baseline content"
    );

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// Verifier (b) F3, through the binary: `u` on an **added** file is a deletion, so it asks
/// — and `n` leaves the file exactly as it was.
///
/// The reducer test (`app_restore_hunk_on_an_added_file_asks_to_delete`) pins the routing;
/// this pins the consequence, which is what the finding was actually about: before the fix
/// this keystroke removed a file the reviewer had never been asked about, and then reported
/// `restored added.txt hunk 1` for a path that no longer existed.
#[test]
fn pty_restore_added_file_asks_then_n_keeps_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    // A file the agent added after first sight: one whole-file insert hunk, no baseline.
    let added = fx.parent.join("alpha/added.txt");
    const BODY: &str = "the agent wrote this\nand this\n";
    std::fs::write(&added, BODY).expect("the agent adds a file");

    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    pty.wait_for_text("A added.txt", LONG)
        .unwrap_or_else(|e| panic!("the added row: {e}"));
    select_until(&mut pty, "added.txt  A");

    // `u` on the row's one content hunk: the question, not the deletion.
    pty.send(b"u").expect("u");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents()
            .contains("Delete added.txt? (added since baseline)")
    })
    .unwrap_or_else(|e| panic!("the delete confirm: {e}"));
    assert!(
        added.exists(),
        "the question is on screen and nothing has been written"
    );

    let t = Instant::now();
    pty.send(b"n").expect("n");
    pty.wait_for(OVERLOADED, |s| {
        !s.contents().contains("Delete added.txt?") && s.contents().contains("A added.txt")
    })
    .unwrap_or_else(|e| panic!("n closes the question and keeps the row: {e}"));
    note(&format!(
        "PTY added-file restore: declined after {:.3?}",
        t.elapsed()
    ));
    assert_eq!(
        std::fs::read_to_string(&added).expect("added.txt is still there"),
        BODY,
        "n kept the file, bytes and all"
    );

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// `m`, a note, Enter — with no herdr to send to. The flag lands in the ledger, the row
/// grows its `⚑`, and the export is appended to the fallback file under the state dir,
/// which is compared against `flag_export_pty.md`.
///
/// This is the only scene that proves the export the *binary* produces: the engine's own
/// golden is written from a unit test with a fixed clock, and neither one alone shows that
/// what the reviewer typed reaches the file they can paste from.
#[test]
fn pty_flag_note_exports_when_standalone() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let alpha = fx.repo("alpha");
    alpha.write("f1", F1_TWO_HUNKS);

    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    pty.send(b"jj\r").expect("keys");
    pty.wait_for(Duration::from_secs(5), |s| {
        let t = s.contents();
        t.contains("f1  M  +2 −2") && t.matches("@@ -").count() == 2
    })
    .unwrap_or_else(|e| panic!("f1 open with two hunks: {e}"));

    // Hunk 2, so the export's `hunk 2 of 2` is not the trivial first one.
    pty.send(b"n").expect("n");
    pty.send(b"m").expect("m");
    pty.wait_for(Duration::from_secs(5), |s| {
        let t = s.contents();
        t.contains("f1 · hunk 2 of 2") && t.contains("⏎ send")
    })
    .unwrap_or_else(|e| panic!("the note modal: {e}"));

    // `q` is a printable character inside the modal, not the quit key: typing the note is
    // the proof that the field swallows the keymap.
    pty.send("this line looks wrong · q".as_bytes())
        .expect("the note");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("this line looks wrong")
    })
    .unwrap_or_else(|e| panic!("the note is echoed: {e}"));
    assert!(
        pty.screen_text().contains("f1 · hunk 2 of 2"),
        "still the modal, not a quit"
    );

    // Verifier (b) F6: the two ways a note gets a second line, through the real crossterm
    // reader rather than through `note_action`. A raw `0x0a` is what a terminal sends for
    // `Ctrl-J`, the modal's newline…
    pty.send(b"\n").expect("ctrl-j");
    pty.send("and so does the next one".as_bytes())
        .expect("line two");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("and so does the next one")
    })
    .unwrap_or_else(|e| panic!("the second line is echoed: {e}"));

    // …and a bracketed paste is one event carrying newlines of its own. This is the
    // failure `tui.md` calls the worst this modal has: firing off the first line and
    // dropping the rest. Nothing is sent until the `\r` below. (The `Ctrl-J` first is the
    // reader opening a line for it; the paste itself brings only its own newline.)
    pty.send(b"\n").expect("ctrl-j");
    pty.send(b"\x1b[200~pasted line 3\npasted line 4\x1b[201~")
        .expect("the paste");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("pasted line 4")
    })
    .unwrap_or_else(|e| panic!("the pasted lines are echoed: {e}"));
    assert!(
        pty.screen_text().contains("f1 · hunk 2 of 2"),
        "still the modal: the paste's newlines did not send it"
    );

    /// The note as the four lines are typed, pasted and echoed above.
    const NOTE: &str =
        "this line looks wrong · q\nand so does the next one\npasted line 3\npasted line 4";

    let t = Instant::now();
    pty.send(b"\r").expect("enter");
    pty.wait_for(OVERLOADED, |s| {
        let (_, cols) = s.size();
        s.rows(0, cols)
            .last()
            .is_some_and(|r| r.contains("flagged f1 hunk 2 · export → "))
    })
    .unwrap_or_else(|e| panic!("the export status: {e}"));
    note(&format!(
        "PTY flag standalone: export status after {:.3?}",
        t.elapsed()
    ));
    // The row keeps its hunks — a flag changes nothing but the ledger — and gains the mark.
    pty.wait_for_text("M f1 ⚑", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("the nav flag marker: {e}"));

    // `flags` is the 1.1 list; `flag` beside it is the 1.0 mirror, which has no hunk field.
    let ledger = fx.ledger("alpha");
    let flags = &ledger["overrides"]["f1"]["flags"];
    assert_eq!(flags.as_array().map(Vec::len), Some(1), "{ledger}");
    assert_eq!(flags[0]["note"].as_str(), Some(NOTE), "{ledger}");
    assert_eq!(flags[0]["hunk"]["index"].as_u64(), Some(1), "{ledger}");
    assert_eq!(
        ledger["overrides"]["f1"]["flag"]["note"].as_str(),
        Some(NOTE),
        "the 1.0 mirror is written too: {ledger}"
    );

    let path = export_file(&fx.state, "alpha");
    let hunk_export = normalise_export(&std::fs::read_to_string(&path).expect("the export file"));
    for line in NOTE.lines() {
        assert!(
            hunk_export.contains(line),
            "every line of the note reaches the export:\n{hunk_export}"
        );
    }

    // Amendment v1.8 / ruling P4: `m` from the **nav** flags the whole file. Esc drops the
    // diff focus first, so the second `m` has no hunk under it. The header says `whole
    // file` where the first one said `hunk 2 of 2`, a summary line follows it, and there is
    // no diff block — the export below is the golden that pins all three.
    // `h` is `back` too (`esc`, `h`, `left`), and it is the one of the three that cannot
    // merge with the key after it: two writes of `\x1b` then `m` still reach a reader
    // that is behind as one buffer, which crossterm parses as `alt-m` — no key at all.
    // That is what PR #9's second macOS CI run saw (the whole-file modal never opened);
    // the wait that stood here checked the note modal's keys line, which the `⏎` above
    // had already closed, so it proved nothing about the Esc. Focus itself shows only
    // as a border colour, so there is nothing textual to wait for between the two keys;
    // none is needed, the app takes them in order.
    pty.send(b"h").expect("h back to the nav");
    pty.send(b"m").expect("m");
    pty.wait_for(Duration::from_secs(5), |s| {
        let t = s.contents();
        t.contains("f1 · whole file") && t.contains("flag whole file")
    })
    .unwrap_or_else(|e| panic!("the whole-file note modal: {e}"));

    /// The second note, the whole-file one.
    const WHOLE: &str = "the whole file needs another pass";

    pty.send(WHOLE.as_bytes()).expect("the whole-file note");
    pty.send(b"\r").expect("enter");
    pty.wait_for(OVERLOADED, |s| {
        let (_, cols) = s.size();
        s.rows(0, cols)
            .last()
            .is_some_and(|r| r.contains("flagged f1 · export → "))
    })
    .unwrap_or_else(|e| panic!("the whole-file export status: {e}"));

    let ledger = fx.ledger("alpha");
    let flags = &ledger["overrides"]["f1"]["flags"];
    assert_eq!(flags.as_array().map(Vec::len), Some(2), "{ledger}");
    assert_eq!(flags[1]["note"].as_str(), Some(WHOLE), "{ledger}");
    assert!(
        flags[1]["hunk"].is_null(),
        "a whole-file flag carries no hunk: {ledger}"
    );
    assert_eq!(flags[1]["summary"]["hunks"].as_u64(), Some(2), "{ledger}");
    assert_eq!(flags[1]["summary"]["added"].as_u64(), Some(2), "{ledger}");
    assert_eq!(flags[1]["summary"]["deleted"].as_u64(), Some(2), "{ledger}");

    let actual = normalise_export(&std::fs::read_to_string(&path).expect("the export file"));
    assert!(
        actual.starts_with(&hunk_export),
        "the whole-file export is appended after the hunk one:\n{actual}"
    );
    if std::env::var_os("LASTCALL_UPDATE_GOLDEN").is_some() {
        std::fs::write(FLAG_EXPORT_PTY_GOLDEN, &actual).expect("write golden");
        note(&format!("golden rewritten: {FLAG_EXPORT_PTY_GOLDEN}"));
    } else {
        let expected = std::fs::read_to_string(FLAG_EXPORT_PTY_GOLDEN).unwrap_or_else(|e| {
            panic!("read {FLAG_EXPORT_PTY_GOLDEN}: {e} (run `just flag-export-golden`)")
        });
        assert!(
            actual == expected,
            "the export file differs from the golden (run `just flag-export-golden` if \
             intended)\n--- expected ---\n{expected}\n--- actual ---\n{actual}"
        );
    }

    // `M` (deliverable 9): the file's flags go away together. The cursor is still on f1
    // from the whole-file flag above, so the key needs nothing else, and the two flags this
    // scene wrote are the only thing it can clear. The **export is not** rewound: the file
    // the reviewer pastes from is an append-only record of what was said, and an unflag is
    // a note about the ledger, not about the conversation.
    let exported = std::fs::read_to_string(&path).expect("the export file");
    pty.send(b"M").expect("M");
    pty.wait_for(Duration::from_secs(5), |s| status_is(s, "flags cleared"))
        .unwrap_or_else(|e| panic!("`M` clears the flags: {e}\n{}", pty.screen_text()));
    pty.wait_for(Duration::from_secs(5), |s| {
        let t = s.contents();
        t.contains("M f1") && !t.contains("M f1 ⚑")
    })
    .unwrap_or_else(|e| panic!("the row keeps its hunks and loses the mark: {e}"));
    let ledger = fx.ledger("alpha");
    assert_eq!(
        ledger["overrides"],
        serde_json::json!({}),
        "both flags are gone, and with them the override entry that held them (the 1.0 \
         mirror included): {ledger}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("the export file"),
        exported,
        "the export file is not rewound by an unflag"
    );

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// Ruling P9, the other half: the scene that **does not** set `LASTCALL_KEYBOARD=plain`.
///
/// Every other PTY scene sets it (`PtyCommand::isolated_lastcall`) so none of them pays
/// crossterm's 2 s probe timeout against a harness that answers nothing. This one removes
/// it and plays the terminal: it waits for the query crossterm writes to `/dev/tty`
/// (`CSI ? u`, then the `CSI c` that bounds it), answers as a kitty-protocol terminal
/// would, and then checks the three things that can go wrong.
///
/// 1. the query is actually written — the probe ran;
/// 2. the app still reaches its first frame, and the enhanced key hint proves the answer
///    was believed and the flags were pushed (`CSI > 1 u` in the transcript);
/// 3. no byte of the reply ever surfaces as a key — crossterm's own reader consumes it
///    before the input thread starts, so the screen carries no `?0u` / `?62` text, no
///    modal opened by itself, and the app is still running to be quit.
#[test]
fn pty_keyboard_enhancement_probe_is_answered_and_swallowed() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let spawned = fx
        .command(&bin())
        .env_remove("LASTCALL_KEYBOARD")
        .args(["tui", "--poll", "1"])
        .spawn();
    let mut pty = match spawned {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
            note(&format!("SKIP: this host cannot open a pty: {e}"));
            return;
        }
        Err(e) => panic!("spawn lastcall tui: {e}"),
    };

    // (1) The probe writes `CSI ? u` (and the `CSI c` whose reply bounds the wait).
    let waited = wait_raw(&mut pty, b"\x1b[?u", Duration::from_secs(5));
    note(&format!(
        "PTY keyboard probe: query written after {waited:.3?}"
    ));
    assert!(
        find(&pty.raw(), b"\x1b[c").is_some(),
        "the device-attributes query that bounds the wait is written too"
    );

    // Answer the way kitty, WezTerm, foot or Ghostty would: the flags report, then DA1.
    pty.send(b"\x1b[?0u\x1b[?62;1;6c").expect("the reply");

    // (2) The app still starts…
    wait_first_piles(&mut pty);
    assert!(
        find(&pty.raw(), b"\x1b[>1u").is_some(),
        "the disambiguate flag is pushed once the probe says yes"
    );
    // …and it believed the answer: the enhanced hint is the one the note modal draws only
    // when `term::keyboard_enhanced()` said yes.
    // `jj` past the root header onto `f1`, then `m` on it.
    pty.send(b"jj").expect("jj");
    pty.wait_for(Duration::from_secs(5), |s| s.contents().contains("f1  M "))
        .unwrap_or_else(|e| panic!("f1 is the selected row: {e}"));
    pty.send(b"m").expect("m");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("⇧⏎ / ^J newline")
    })
    .unwrap_or_else(|e| panic!("the enhanced note-modal hint: {e}"));

    // (3) Nothing of the reply reached the app as input.
    pty.send(b"\x1b").expect("esc");
    pty.wait_for(Duration::from_secs(5), |s| !s.contents().contains("⏎ send"))
        .unwrap_or_else(|e| panic!("esc closes the modal: {e}"));
    let screen = pty.screen_text();
    for stray in ["?0u", "62;1;6", "[?62"] {
        assert!(
            !screen.contains(stray),
            "no reply byte was echoed as typing ({stray}):\n{screen}"
        );
    }

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert!(
        find(&pty.raw()[since..], b"\x1b[<1u").is_some(),
        "the pushed flags are popped before the alternate screen is left"
    );
    assert_clean_exit(&pty, since);
}

// ---- Phase 8 deliverable 7: `$EDITOR` -----------------------------------------------------

/// The probe editor (`tests/probe/editor.sh`) as a symlink named `vim` inside `dir`, plus
/// the log it appends to. The **symlink's** name is what `editor.rs`'s basename table keys
/// off, so a scene reaching this gets `+<line> <file>` argv; the absolute path is what
/// `$EDITOR` is set to, so nothing goes on `PATH` and no editor of the developer's is
/// reachable (`isolated_lastcall` removed `$VISUAL` and `$EDITOR` outright).
fn probe_vim(dir: &Path) -> (PathBuf, PathBuf) {
    let bindir = dir.join("probe-bin");
    std::fs::create_dir_all(&bindir).expect("the probe bin dir");
    let vim = bindir.join("vim");
    if !vim.exists() {
        std::os::unix::fs::symlink(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/probe/editor.sh"),
            &vim,
        )
        .expect("the `vim` symlink");
    }
    (vim, dir.join("editor.log"))
}

/// What the probe editor "saves": `PARSE_RS_EDITED` with one more line **inside the middle
/// hunk**, right under the line `shift-i` puts the cursor on. That placement is the point:
/// the blessing has to cover the whole live file, not just the hunk the editor was opened
/// at, and a line somewhere else would not tell those two apart from the row alone.
fn parse_rs_saved() -> String {
    let mut out = String::new();
    for line in fixture_parent::PARSE_RS_EDITED.lines() {
        out.push_str(line);
        out.push('\n');
        if line == fixture_parent::PARSE_RS_EDIT2 {
            out.push_str("            // typed in $EDITOR\n");
        }
    }
    assert!(
        out.len() > fixture_parent::PARSE_RS_EDITED.len(),
        "the marker line is in the edited text"
    );
    out
}

/// Move the nav selection to `parse.rs`, open it, and step to its **second** hunk — the one
/// with real leading context (design review F5, F9). `jjjj` walks alpha's nav
/// (root, f1, f2, src/parse.rs); `⏎` focuses the diff, which is where `shift-i` reads the
/// hunk under the cursor from.
fn open_parse_rs_hunk_2(pty: &mut PtyTui) {
    pty.wait_for_text("M parse.rs  +10 −2", LONG)
        .unwrap_or_else(|e| panic!("the parse.rs row: {e}"));
    pty.send(b"jjjj\r").expect("keys");
    pty.wait_for_text("parse.rs  M  +10 −2", LONG)
        .unwrap_or_else(|e| panic!("parse.rs opens in the diff pane: {e}"));
    let first = hunk_headers(pty)
        .first()
        .map(|(_, r)| header_text(r))
        .expect("a hunk header on screen");
    assert!(
        first.starts_with("@@ -1,"),
        "hunk 1 is the module doc at the top of the file, not {first}"
    );
    pty.send(b"n").expect("n");
    pty.wait_for(Duration::from_secs(5), |s| {
        let (_, cols) = s.size();
        s.rows(0, cols)
            .find(|r| r.contains("@@ -"))
            .is_some_and(|r| !header_text(&r).starts_with("@@ -1,"))
    })
    .unwrap_or_else(|e| panic!("`n` scrolls hunk 2's header to the top: {e}"));
}

/// Gate item 2 end to end: an `$EDITOR` session that **saves** leaves nothing pending — the
/// user is asked whether they meant it, and `Enter` blesses the whole live file.
///
/// Both halves of ruling P1 are here, in one process because that is the only way to show
/// they are the same question answered differently:
///
/// * `Enter` on `alpha/src/parse.rs` — the row goes away entirely (invariant 8 for the
///   session: what the editor left behind *is* the reviewed content, not just the hunk that
///   was open), and the file on disk holds the line the editor wrote;
/// * `Esc` on `alpha/f1`, whose row the same probe editor also rewrites — the row **stays**,
///   with the editor's own change pending on it like any agent's.
///
/// The second half is on a different file for a plain reason: after the first one is blessed
/// there is no `parse.rs` row left to press `shift-i` on.
#[test]
fn pty_editor_save_pends_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let (vim, log) = probe_vim(&fx.state);
    let saved = parse_rs_saved();
    let Some(mut pty) = fx.spawn_tui_env(
        &bin(),
        &[
            ("EDITOR", vim.into_os_string()),
            ("LASTCALL_PROBE_EDITOR_LOG", log.clone().into_os_string()),
            ("LASTCALL_PROBE_EDITOR_WRITE", saved.clone().into()),
        ],
    ) else {
        return;
    };
    wait_first_piles(&mut pty);
    // Before `shift-i`, not for the timing: the watcher emits its `watching <parent>
    // (3 roots)` notice once, and a notice is a **status**, so one that arrived while the
    // editor had the terminal would drain over the return path's own status the moment the
    // loop ran again. Waiting for it here is the scene saying which status it is reading.
    wait_watching(&mut pty);

    // (1) `shift-i` on parse.rs hunk 2; the probe rewrites the file and exits.
    open_parse_rs_hunk_2(&mut pty);
    let t = Instant::now();
    pty.send(b"I").expect("shift-i");
    pty.wait_for_text("mark every hunk in it reviewed?", LONG)
        .unwrap_or_else(|e| panic!("the blessing confirm after the editor exits: {e}"));
    note(&format!(
        "PTY editor: save to confirm in {:.3?}",
        t.elapsed()
    ));
    assert_eq!(
        std::fs::read_to_string(fx.parent.join("alpha").join(fixture_parent::PARSE_RS))
            .expect("parse.rs"),
        saved,
        "the editor's save is on disk before the question is answered"
    );

    // `Enter` = yes: the whole live file is accepted, so the row is gone.
    pty.send(b"\r").expect("enter");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "reviewed src/parse.rs") && !s.contents().contains("M parse.rs")
    })
    .unwrap_or_else(|e| panic!("Enter blesses the session's content: {e}"));
    assert_eq!(
        std::fs::read_to_string(fx.parent.join("alpha").join(fixture_parent::PARSE_RS))
            .expect("parse.rs"),
        saved,
        "blessing is metadata: the bytes the editor wrote are untouched"
    );

    // (2) the same editor on `f1`, answered `Esc`: the row stays, pending the editor's own
    // change. Nothing is written by lastcall either way — this is the half that shows the
    // confirm is a real question and not a formality.
    select_until(&mut pty, "f1  M");
    pty.send(b"\r").expect("open f1");
    pty.wait_for_text("@@ -", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("f1's diff: {e}"));
    pty.send(b"I").expect("shift-i");
    pty.wait_for_text("f1 edited — mark every hunk in it reviewed?", LONG)
        .unwrap_or_else(|e| panic!("the blessing confirm for f1: {e}"));
    pty.send(b"\x1b").expect("esc");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "f1 left pending") && s.contents().contains("M f1")
    })
    .unwrap_or_else(|e| panic!("Esc leaves the row pending: {e}"));
    assert_eq!(
        std::fs::read_to_string(fx.parent.join("alpha/f1")).expect("f1"),
        saved,
        "declining changes nothing on disk either"
    );

    // The probe ran twice, and both times through the `vim` symlink at an absolute path.
    let logged = std::fs::read_to_string(&log).expect("the probe log");
    assert_eq!(
        logged.lines().filter(|l| l.starts_with("argv: +")).count(),
        2,
        "two editor sessions with a `+<line>` argv:\n{logged}"
    );

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// Design review F4: a `^C` typed while `$EDITOR` owns the terminal must not quit lastcall.
///
/// During the suspend the tty is back in cooked mode, so the `\x03` is not a key event —
/// the line discipline turns it into a `SIGINT` for the whole foreground process group,
/// which is the editor **and** lastcall. The editor dies from it (that is what a user
/// pressing `^C` in `vim` expects); lastcall's tokio handler latches it, and `Signals::
/// resume` is what stops the loop reading that latch as "quit" the moment it runs again.
///
/// Nothing here can be proved from a reducer: the signal never reaches the reducer, and the
/// only observable is that the frame comes back instead of the process ending.
#[test]
fn pty_editor_ctrl_c_does_not_quit_lastcall() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let (vim, log) = probe_vim(&fx.state);
    let Some(mut pty) = fx.spawn_tui_env(
        &bin(),
        &[
            ("EDITOR", vim.into_os_string()),
            ("LASTCALL_PROBE_EDITOR_LOG", log.clone().into_os_string()),
            // Long enough that the `^C` lands while the script is still running, short
            // enough that a `^C` that never arrives does not hang the scene.
            ("LASTCALL_PROBE_EDITOR_SLEEP", "1".into()),
        ],
    ) else {
        return;
    };
    wait_first_piles(&mut pty);
    // The watcher's one-shot `watching …` notice, before the suspend rather than during it
    // (see `pty_editor_save_pends_nothing`).
    wait_watching(&mut pty);
    open_parse_rs_hunk_2(&mut pty);

    pty.send(b"I").expect("shift-i");
    // The probe writes its log line before it sleeps, so this is "the editor has the
    // terminal now" — no fixed sleep on our side.
    let start = Instant::now();
    loop {
        if std::fs::read_to_string(&log).is_ok_and(|t| t.contains("cwd: ")) {
            break;
        }
        assert!(
            start.elapsed() < LONG,
            "the probe editor never started:\n{}",
            pty.screen_text()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    pty.send(b"\x03").expect("^C at the editor");

    // The script wrote nothing, so the return path's answer is `no change` — and its being
    // on screen at all is the assertion: lastcall is alive, back on the alternate screen,
    // with the row it left.
    pty.wait_for(LONG, |s| status_is(s, "no change"))
        .unwrap_or_else(|e| panic!("the TUI is back after the ^C: {e}"));
    let back = start.elapsed();
    note(&format!("PTY editor ^C: frame back after {back:.3?}"));
    // The probe was told to sleep a second. Coming back sooner is the proof that the `^C`
    // reached the *child* — a sleep that simply finished would look the same on screen.
    assert!(
        back < Duration::from_secs(1),
        "the ^C interrupted the editor rather than the sleep running out ({back:?})"
    );
    assert!(
        pty.screen(|s| s.alternate_screen()),
        "back on the alternate screen"
    );
    assert!(
        pty.screen_text().contains("parse.rs  M  +10 −2"),
        "the same row is still open:\n{}",
        pty.screen_text()
    );

    // …and the keyboard still reaches it — with a **second `^C`**, which is the half of the
    // claim the scene used to leave to a `q` (verifier (b) F3). It proves two things at
    // once: `Signals::resume`'s drain swallowed the editor's interrupt and not this one,
    // and the resumed terminal is back in raw mode, where `\x03` is a key event the keymap
    // quits on rather than a signal. It goes out well past `EDITOR_SETTLE` (50 ms), which
    // is the window the docs promise a `ctrl-c` has to be repeated in.
    std::thread::sleep(Duration::from_millis(300));
    let since = pty.raw().len();
    pty.send(b"\x03").expect("^C after the resume");
    let status = pty
        .wait_exit(QUIT_BUDGET)
        .expect("a real ^C a moment after the resume quits");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// Verifier (b) F1: a key typed while `$EDITOR` owns the terminal must act on the **resume**
/// and not wait for a second key.
///
/// The bug it guards is below crossterm and macOS-only. crossterm registers the tty with
/// kqueue once per process and edge-triggered (`EV_CLEAR`); xnu's raw-mode switch moves the
/// pending cooked line into the raw queue without waking anyone, so the byte is readable and
/// never reported — verifier (b) watched `poll` return `Ok(false)` 134 times over 6.7 s with
/// a `n` sitting in the queue, and the *next* key delivered both at once. `Suspend::run`
/// therefore writes one `ESC [ 6 n` after `term::enter()`: the terminal's reply is an edge
/// the kqueue does fire on, and the read it wakes drains the stuck byte with it.
///
/// So the scene needs a terminal that answers — `PtyCommand::answer_cursor_position`, which
/// no other scene turns on. On a terminal that stays silent the byte still waits for the
/// next key; that residual is in `tui.md`'s suspend step list, and it is why this scene
/// cannot be written without the harness switch.
///
/// `n` is the key on purpose: one printable byte, so nothing here depends on how the line
/// discipline treats `\r` or on crossterm's lone-`ESC` disambiguation.
#[test]
fn pty_editor_key_typed_during_the_editor_is_not_stuck() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let (vim, log) = probe_vim(&fx.state);
    let saved = parse_rs_saved();
    let spawned = fx
        .command(&bin())
        .args(["tui", "--poll", "1"])
        .env("EDITOR", vim.into_os_string())
        .env("LASTCALL_PROBE_EDITOR_LOG", log.clone().into_os_string())
        // The editor saves, so the return path opens the blessing confirm — the `n` typed
        // during the sleep is the answer to it.
        .env("LASTCALL_PROBE_EDITOR_WRITE", saved.clone())
        .env("LASTCALL_PROBE_EDITOR_SLEEP", "1")
        .answer_cursor_position()
        .spawn();
    let mut pty = match spawned {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
            note(&format!("SKIP: this host cannot open a pty: {e}"));
            return;
        }
        Err(e) => panic!("spawn lastcall tui: {e}"),
    };
    wait_first_piles(&mut pty);
    wait_watching(&mut pty);
    open_parse_rs_hunk_2(&mut pty);

    pty.send(b"I").expect("shift-i");
    // The probe logs `cwd:` before it sleeps: the editor owns the terminal from here.
    let start = Instant::now();
    loop {
        if std::fs::read_to_string(&log).is_ok_and(|t| t.contains("cwd: ")) {
            break;
        }
        assert!(
            start.elapsed() < LONG,
            "the probe editor never started:\n{}",
            pty.screen_text()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    pty.send(b"n").expect("n while the editor sleeps");

    // …and nothing else is ever sent. Before the fix this waited forever: the confirm sat
    // on screen unanswered until a second key arrived.
    let answered = pty
        .wait_for(Duration::from_secs(5), |s| {
            status_is(s, "src/parse.rs left pending")
        })
        .unwrap_or_else(|e| {
            panic!("the `n` typed during the editor answers the confirm on the resume: {e}")
        });
    note(&format!(
        "PTY editor stuck key: answered {answered:.3?} after the send"
    ));
    let screen = pty.screen_text();
    assert!(
        !screen.contains("mark every hunk in it reviewed?"),
        "the confirm is gone:\n{screen}"
    );
    assert!(
        screen.contains("M parse.rs"),
        "and the row is still pending, which is what `n` means:\n{screen}"
    );
    assert_eq!(
        std::fs::read_to_string(fx.parent.join("alpha").join(fixture_parent::PARSE_RS))
            .expect("parse.rs"),
        saved,
        "declining wrote nothing: the editor's bytes are what is on disk"
    );

    // The mechanism, on the wire: exactly one cursor-position request, written by the
    // resume and by nothing else (`LASTCALL_KEYBOARD=plain` skips the other query lastcall
    // knows how to write).
    let raw = pty.raw();
    assert_eq!(
        raw.windows(4).filter(|w| *w == b"\x1b[6n").count(),
        1,
        "one DSR nudge, from the one suspend"
    );

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// The inline editor's header, as [`render::render_editor_header`] writes it for the middle
/// hunk of `src/parse.rs` — the line number comes from the fixture text, not from the
/// engine under test.
fn editing_parse_rs() -> String {
    format!(
        "editing src/parse.rs · line {}/",
        fixture_parent::parse_rs_edit2_line()
    )
}

/// Gate item 1 end to end: `i` opens the file **in** lastcall, typed and pasted text lands
/// in the buffer, and `Ctrl-S` writes it under the same compare-and-swap an accept uses —
/// after which the row is gone with nothing left pending, because the bytes on disk are the
/// bytes the ledger just blessed.
///
/// Two files, because they are two different round trips:
///
/// * `alpha/src/parse.rs`, which ends in a newline: typing, a **bracketed paste** of two
///   lines, then `^S`. Every character is on disk and the row is gone.
/// * `notes/n2.md`, rewritten **without** a trailing newline before the child starts
///   (design review F6): saving it back must not grow one. A buffer that quietly terminates
///   the last line rewrites a file the reader never touched that way, and the diff the next
///   agent sees would be lastcall's, not theirs.
#[test]
fn pty_edit_inline_save_pends_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    // The agent's edit to the draft note, minus the trailing newline.
    let n2 = fx.parent.join("notes/n2.md");
    std::fs::write(&n2, "# note 2\n\nedited").expect("the no-EOL draft");

    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    // The watcher's one-shot `watching <parent> (3 roots)` notice is a status; waiting for
    // it here means the `saved …` status below is the one this scene put there.
    wait_watching(&mut pty);

    // (1) parse.rs, opened at the middle hunk.
    open_parse_rs_hunk_2(&mut pty);
    let t = Instant::now();
    pty.send(b"i").expect("i");
    pty.wait_for_text(&editing_parse_rs(), LONG)
        .unwrap_or_else(|e| panic!("the inline editor opens at the hunk's line: {e}"));
    note(&format!("PTY inline editor: open in {:.3?}", t.elapsed()));

    // Type a line, then paste two more as one bracketed paste.
    pty.send(b"// typed inline\r").expect("typing");
    pty.wait_for_text("// typed inline", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("the typed line is in the buffer: {e}"));
    pty.send(b"\x1b[200~// pasted one\n// pasted two\n\x1b[201~")
        .expect("paste");
    pty.wait_for_text("// pasted two", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("the pasted block is in the buffer: {e}"));

    let t = Instant::now();
    pty.send(b"\x13").expect("ctrl-s");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "saved src/parse.rs") && !s.contents().contains("M parse.rs")
    })
    .unwrap_or_else(|e| panic!("^S saves and the row goes away: {e}"));
    note(&format!(
        "PTY inline editor: ^S to saved in {:.3?}",
        t.elapsed()
    ));

    let on_disk = std::fs::read_to_string(fx.parent.join("alpha").join(fixture_parent::PARSE_RS))
        .expect("parse.rs");
    for line in ["// typed inline", "// pasted one", "// pasted two"] {
        assert!(on_disk.contains(line), "{line} is on disk:\n{on_disk}");
    }
    assert!(
        on_disk.contains(fixture_parent::PARSE_RS_EDIT2),
        "and the agent's own line survived the round trip"
    );
    assert!(
        !pty.screen_text().contains("editing src/parse.rs"),
        "a clean save closes the editor:\n{}",
        pty.screen_text()
    );

    // (2) the draft note with no trailing newline. `select_until` takes the focus back to
    // the nav itself: the save left it in the diff pane, where `j` scrolls.
    select_until(&mut pty, "n2.md  M");
    pty.send(b"\r").expect("open n2.md");
    pty.wait_for_text("@@ -", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("n2.md's diff: {e}"));
    pty.send(b"i").expect("i");
    pty.wait_for_text("editing n2.md · line", LONG)
        .unwrap_or_else(|e| panic!("the inline editor on the draft note: {e}"));
    pty.send(b"more ").expect("typing");
    pty.wait_for_text("more ", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("the typed text is in the buffer: {e}"));
    pty.send(b"\x13").expect("ctrl-s");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "saved n2.md") && !s.contents().contains("M n2.md")
    })
    .unwrap_or_else(|e| panic!("^S saves the draft note: {e}"));

    let note_text = std::fs::read_to_string(&n2).expect("n2.md");
    assert!(
        note_text.contains("more "),
        "the typed text is on disk: {note_text:?}"
    );
    assert!(
        !note_text.ends_with('\n'),
        "saving a file with no trailing newline must not grow one (F6): {note_text:?}"
    );

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// The save's compare-and-swap on screen: an agent writes the file while the reader is
/// typing in it, and `^S` refuses. Nothing is written, **every character the reader typed
/// stays in the buffer** — throwing their work away to tell them the file moved would be
/// the worst possible reading of "not saved" — and the status names the two keys that
/// reload.
#[test]
fn pty_edit_inline_save_refused_when_the_file_moved() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    wait_watching(&mut pty);

    pty.send(b"jj\r").expect("keys");
    pty.wait_for_text("f1  M  +1 −1", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("f1 open: {e}"));
    pty.send(b"i").expect("i");
    pty.wait_for_text("editing f1 · line", LONG)
        .unwrap_or_else(|e| panic!("the inline editor on f1: {e}"));
    pty.send(b"typed but never saved ").expect("typing");
    pty.wait_for_text("typed but never saved", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("the typed text is in the buffer: {e}"));

    // The agent writes f1 underneath the open buffer.
    fx.append("alpha/f1", "moved while the editor was open\n");
    let before = std::fs::read(fx.parent.join("alpha/f1")).expect("read f1");

    let t = Instant::now();
    pty.send(b"\x13").expect("ctrl-s");
    pty.wait_for(OVERLOADED, |s| {
        status_is(
            s,
            "f1: changed since you opened it; not saved — Esc, then i to reload",
        )
    })
    .unwrap_or_else(|e| panic!("the refusal status: {e}"));
    note(&format!(
        "PTY inline save refused: status after {:.3?}",
        t.elapsed()
    ));
    assert_eq!(
        std::fs::read(fx.parent.join("alpha/f1")).expect("read f1"),
        before,
        "a refused save writes nothing at all"
    );
    let screen = pty.screen_text();
    assert!(
        screen.contains("editing f1 · line") && screen.contains("typed but never saved"),
        "the buffer is kept whole:\n{screen}"
    );

    // Esc asks before throwing the text away; `y` is the only thing that does.
    pty.send(b"\x1b").expect("esc");
    pty.wait_for_text("Discard changes to f1?", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("the discard question: {e}"));
    pty.send(b"y").expect("y");
    pty.wait_for(OVERLOADED, |s| {
        !s.contents().contains("editing f1") && s.contents().contains("M f1")
    })
    .unwrap_or_else(|e| panic!("the editor closes and the row is still pending: {e}"));

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// RFC 4648 base64, decoded independently of the encoder the scene is checking (verifier
/// (b) F6). Padding is trusted to be well formed — the input is one OSC 52 payload lastcall
/// just wrote — and any byte outside the alphabet is a failure, not a skip.
fn base64_decode(s: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let digits: Vec<u8> = s
        .bytes()
        .take_while(|b| *b != b'=')
        .map(|b| {
            ALPHABET
                .iter()
                .position(|a| *a == b)
                .unwrap_or_else(|| panic!("{:?} is not base64", b as char)) as u8
        })
        .collect();
    let mut out = Vec::with_capacity(digits.len() * 3 / 4);
    for quad in digits.chunks(4) {
        let n = quad
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, d)| acc | (*d as u32) << (18 - 6 * i));
        // A 4-digit group carries three bytes, a 3-digit group two, a 2-digit group one.
        for i in 0..quad.len() - 1 {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}

/// Deliverable 9 end to end: `v j j` selects three diff lines, `y` copies them, and the one
/// thing lastcall can actually prove about a clipboard over ssh is on the wire — a single
/// OSC 52 write whose base64 is the three lines as the pane drew them.
///
/// OSC 52 is write-only: no terminal answers it, so "the clipboard now holds this" is not
/// a claim any test can make. The sponsor's own check at the PR is the other half (design
/// review F14).
#[test]
fn pty_copy_writes_osc52_with_the_selected_lines() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    wait_watching(&mut pty);
    open_parse_rs_hunk_2(&mut pty);

    // The three lines the pane is about to hand over: the hunk header `n` scrolled to the
    // top, and the two under it.
    let top = hunk_headers(&pty)
        .first()
        .map(|(row, _)| *row)
        .expect("hunk 2's header at the top of the pane");
    let rows = pty.rows();
    let pane = col_of(&rows[top as usize], "@@ -").expect("the diff pane's left edge");
    let on_screen: Vec<String> = (top..top + 3)
        .map(|r| {
            let text: String = rows[r as usize].chars().skip(pane as usize).collect();
            // The pane's right border is part of the screen row, not of the diff line.
            text.trim_end().trim_end_matches('│').trim_end().to_owned()
        })
        .collect();

    let before = pty.raw().len();
    pty.send(b"vjjy").expect("v j j y");
    pty.wait_for_text("copied to clipboard", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("the cue says the copy happened: {e}"));

    // Exactly one OSC 52 write, and it is this copy's.
    let raw = pty.raw();
    let osc: Vec<usize> = (0..raw.len())
        .filter(|i| raw[*i..].starts_with(b"\x1b]52;c;"))
        .collect();
    assert_eq!(osc.len(), 1, "one OSC 52 write, at {osc:?}");
    assert!(osc[0] >= before, "and it is the one this scene asked for");
    let payload = &raw[osc[0] + b"\x1b]52;c;".len()..];
    let end = payload.iter().position(|b| *b == 0x07).expect("the BEL");
    let encoded = String::from_utf8(payload[..end].to_vec()).expect("base64 is ascii");

    // The first line is the header the screen shows; the two after it are the pane's own
    // lines, `+`/`-`/space and all. **Decoded** here rather than re-encoded (verifier (b)
    // F6): comparing against `clipboard::base64` would be comparing the encoder under test
    // with itself, and an encoder that is wrong the same way twice would pass.
    let header = header_text(&rows[top as usize]);
    assert!(
        on_screen[0].starts_with(&header) && on_screen[0].ends_with("[m flag]"),
        "the header row carries its controls, and they are not part of the line: {:?}",
        on_screen[0]
    );
    let expected = format!("{header}\n{}\n{}\n", on_screen[1], on_screen[2]);
    assert_eq!(
        String::from_utf8(base64_decode(&encoded)).expect("the payload is the pane's text"),
        expected,
        "the payload decodes to the rows that were on screen (encoded: {encoded})"
    );
    assert!(
        expected.len() <= lastcall::tui::clipboard::CAP,
        "well under the 32 KiB cap"
    );

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

// ---- Phase 9b deliverable 2.6: the once-a-day update check --------------------------------

/// A served release directory holding just the release answer (the notice needs nothing
/// else), plus the path the probe `curl` logs its URLs to.
///
/// Both API shapes are written: `releases/latest` answers with the object and
/// `releases?per_page=N` with a one-element array, and which one the binary asks for
/// depends on whether this build's own version is a prerelease
/// (`commands/update.rs::lookup`). The crate version crosses that line during a release, so
/// serving both is what keeps this scene from depending on which side of it we are on.
fn served_release(fx: &Fixture, tag: &str) -> (PathBuf, PathBuf) {
    let serve = fx.state.join("serve");
    std::fs::create_dir_all(&serve).expect("the served dir");
    let one = format!(r#"{{"tag_name":"{tag}","prerelease":false}}"#);
    std::fs::write(serve.join("latest.json"), &one).expect("latest.json");
    std::fs::write(serve.join("list.json"), format!("[{one}]")).expect("list.json");
    (serve, fx.state.join("curl.log"))
}

/// `lastcall tui --poll 1` with the background check turned on or off, pointed at a served
/// release directory. The probe `curl` is already first on `PATH` for every scene
/// (`isolated_lastcall`), so nothing here can reach the network whatever `check` says.
fn spawn_update_tui(fx: &Fixture, check: bool, serve: &Path, log: &Path) -> Option<PtyTui> {
    let cmd = fx
        .command(&bin())
        .update_check(check)
        .args(["tui", "--poll", "1"])
        .env("LASTCALL_TEST_RELEASE_DIR", serve)
        .env("LASTCALL_PROBE_CURL_LOG", log)
        // Set so the "the daily check never reads LASTCALL_UPDATE_BASE_URL" assertion below
        // has something to be true about: a loopback port that answers nothing, so a check
        // that did read it would fail to connect instead of quietly passing (verifier (a) F8).
        .env("LASTCALL_UPDATE_BASE_URL", "http://127.0.0.1:1/");
    match cmd.spawn() {
        Ok(p) => Some(p),
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
            note(&format!("SKIP: this host cannot open a pty: {e}"));
            None
        }
        Err(e) => panic!("spawn lastcall tui: {e}"),
    }
}

fn lookups(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("url: ").map(str::to_owned))
        .collect()
}

fn stamp_of(fx: &Fixture) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(fx.state.join("update-check.json")).ok()?).ok()
}

fn write_stamp(fx: &Fixture, age_hours: u64, latest: Option<&str>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs();
    let stamp = serde_json::json!({
        "checked_at": now - age_hours * 3600,
        "latest": latest,
        "seen_version": env!("CARGO_PKG_VERSION"),
    });
    std::fs::write(
        fx.state.join("update-check.json"),
        serde_json::to_vec(&stamp).expect("json"),
    )
    .expect("the stamp");
}

/// The header's `↑ <version>` on the top row, with the column it starts at.
fn update_notice(s: &vt100::Screen) -> Option<(String, u16)> {
    let (_, cols) = s.size();
    let row = s.rows(0, cols).next()?;
    let at = row.find("↑ ")?;
    // The pad that follows is two or more spaces; everything left of the arrow on this row is
    // ASCII, so the byte offset is also the column.
    let text = row[at..].split("  ").next()?.trim_end().to_owned();
    Some((text, at as u16))
}

/// Design pass D6 and kickoff deliverable 2.6: a newer release puts seven columns in the
/// header, **after the launch hold and never before the first frame**, and a click reads the
/// whole sentence onto the status line. Nothing is said automatically, nothing is written to
/// the working tree, and the check spends exactly one request.
#[test]
fn pty_update_notice_after_hold() {
    let fx = Fixture::build();
    let (serve, log) = served_release(&fx, "v9.9.9");
    let Some(mut pty) = spawn_update_tui(&fx, true, &serve, &log) else {
        return;
    };
    wait_first_piles(&mut pty);
    // The hold is over before the check is even started, so the notice cannot be on the
    // first frame; the raw transcript is the proof, not the timing.
    let raw = pty.raw();
    let hold = find_words(&raw, &["discovered", "3", "repos,", "checking", "status…"])
        .expect("the launch hold");
    assert!(
        find(&raw, "↑ 9.9.9".as_bytes()).is_none_or(|i| i > hold),
        "the notice never precedes the hold"
    );

    pty.wait_for(LONG, |s| update_notice(s).is_some())
        .unwrap_or_else(|e| panic!("the update notice: {e}"));
    let (text, col) = pty.screen(update_notice).expect("the notice");
    assert_eq!(text, "↑ 9.9.9");
    let header = pty.screen(|s| {
        let (_, cols) = s.size();
        s.rows(0, cols).next().unwrap_or_default()
    });
    assert!(header.contains("[Accept All]  ↑ 9.9.9"), "{header}");

    // Click-to-read: the sentence lands on the status line only because the reader asked.
    pty.click(col, 0).expect("click the notice");
    pty.wait_for(LONG, |s| {
        status_is(s, "lastcall 9.9.9 available — run: lastcall update")
    })
    .unwrap_or_else(|e| panic!("the sentence on the status line: {e}"));

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);

    // One request, and the stamp that throttles the next one.
    let urls = lookups(&log);
    assert_eq!(urls.len(), 1, "{urls:?}");
    // `releases/latest` for a stable build, the paged list for a prerelease one: the crate
    // version crosses that line during a release, and the scene serves both shapes.
    let api_path = if env!("CARGO_PKG_VERSION").contains('-') {
        "/releases?per_page=10"
    } else {
        "/releases/latest"
    };
    assert!(urls[0].ends_with(api_path), "{urls:?}");
    assert!(
        urls[0].starts_with("https://api.github.com/"),
        "the daily check never reads LASTCALL_UPDATE_BASE_URL: {urls:?}"
    );
    let stamp = stamp_of(&fx).expect("the stamp was written");
    assert_eq!(stamp["latest"], serde_json::json!("9.9.9"), "{stamp}");
    assert_eq!(
        stamp["seen_version"],
        serde_json::json!(env!("CARGO_PKG_VERSION")),
        "{stamp}"
    );
}

/// The stamp is what makes it once a **day**: an hour-old stamp answers from disk and spends
/// no request, a twenty-five-hour-old one looks again. Both are asserted after the child has
/// exited, so nothing is racing a detached thread.
#[test]
fn pty_update_check_is_throttled_by_the_daily_stamp() {
    // 1 h old, and it already knows about 9.9.9: the notice shows, nothing is fetched.
    let fresh = Fixture::build();
    let (serve, log) = served_release(&fresh, "v9.9.9");
    write_stamp(&fresh, 1, Some("9.9.9"));
    let before = stamp_of(&fresh).expect("the stamp");
    let Some(mut pty) = spawn_update_tui(&fresh, true, &serve, &log) else {
        return;
    };
    wait_first_piles(&mut pty);
    pty.wait_for(LONG, |s| update_notice(s).is_some())
        .unwrap_or_else(|e| panic!("the notice comes from the stamp: {e}"));
    pty.send(b"q").expect("q");
    assert_eq!(pty.wait_exit(QUIT_BUDGET).expect("exits").exit_code(), 0);
    assert_eq!(
        lookups(&log),
        Vec::<String>::new(),
        "inside the day: no lookup"
    );
    assert_eq!(stamp_of(&fresh), Some(before), "the stamp is not rewritten");

    // 25 h old: the lookup runs again and the stamp moves forward.
    let stale = Fixture::build();
    let (serve, log) = served_release(&stale, "v9.9.9");
    write_stamp(&stale, 25, None);
    let before = stamp_of(&stale).expect("the stamp");
    let Some(mut pty) = spawn_update_tui(&stale, true, &serve, &log) else {
        return;
    };
    wait_first_piles(&mut pty);
    pty.wait_for(LONG, |s| update_notice(s).is_some())
        .unwrap_or_else(|e| panic!("the stale stamp is looked past: {e}"));
    pty.send(b"q").expect("q");
    assert_eq!(pty.wait_exit(QUIT_BUDGET).expect("exits").exit_code(), 0);
    assert_eq!(lookups(&log).len(), 1, "{:?}", lookups(&log));
    let after = stamp_of(&stale).expect("the stamp");
    assert!(
        after["checked_at"].as_u64() > before["checked_at"].as_u64(),
        "{before} → {after}"
    );
    assert_eq!(after["latest"], serde_json::json!("9.9.9"), "{after}");
}

/// The harness's own guard: every other scene runs with `[update] check = false` written
/// into its isolated config, so a scene that never thought about releases starts no thread,
/// spends no request and leaves no stamp. Asserted after the child exits.
#[test]
fn pty_update_check_is_off_for_every_other_scene() {
    let fx = Fixture::build();
    let (serve, log) = served_release(&fx, "v9.9.9");
    let Some(mut pty) = spawn_update_tui(&fx, false, &serve, &log) else {
        return;
    };
    wait_first_piles(&mut pty);
    assert!(
        pty.screen(update_notice).is_none(),
        "no notice with the check off"
    );
    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    assert_eq!(pty.wait_exit(QUIT_BUDGET).expect("exits").exit_code(), 0);
    assert_clean_exit(&pty, since);
    assert_eq!(lookups(&log), Vec::<String>::new());
    assert_eq!(stamp_of(&fx), None, "no stamp was written");
    let config = std::fs::read_to_string(&fx.config).expect("the isolated config");
    assert!(config.contains("[update]"), "{config}");
    assert!(config.contains("check = false"), "{config}");
}

// --- Phase 9b deliverable 9: the keys the terminal had not yet reached -------------------
//
// What "complete" means for this tier (`docs/dev/testing.md`, "The e2e tier"): every action
// in `input::DEFAULT_KEYMAP`, every `MODAL_KEYS` answer and every modal is reached by at
// least one scene **through the terminal**. The scenes below close the list Phase 9b
// derived by grepping the keymap against the scenes above: the help overlay, the page keys,
// `Tab`, `p`/`[`, `e`, `f`, `o`, `M`, `w`, `r`, the confirm's `n`, the agent picker, the nav
// divider drag, and a new root only the `--poll` backstop can find. Like every scene here
// they wait on a rendered marker, never on time.

/// The nav row carrying the cursor, trimmed: the selected entry is drawn edge to edge in
/// reverse video, so the first column inside the nav's left border finds it. `None` when
/// nothing is selected, which is how every scene starts.
fn nav_cursor(s: &vt100::Screen) -> Option<String> {
    let (rows, cols) = s.size();
    let divider = divider_col(s)? as usize;
    // The panes' body only: the header (row 0), the borders and the hint line are not nav
    // rows, and the header's `[Accept All]` is drawn inverted.
    let row =
        (2..rows.saturating_sub(2)).find(|r| s.cell(*r, 1).is_some_and(vt100::Cell::inverse))?;
    let text = s.rows(0, cols).nth(row as usize)?;
    Some(
        text.chars()
            .take(divider)
            .skip(1)
            .collect::<String>()
            .trim()
            .to_owned(),
    )
}

/// The column the nav/diff divider is drawn in: the `┬` junction on the panes' top border
/// row. It is one less than `App::nav_width`, which is what a divider drag moves.
fn divider_col(s: &vt100::Screen) -> Option<u16> {
    let (_, cols) = s.size();
    col_of(&s.rows(0, cols).nth(1)?, "┬")
}

/// Whether a cell is drawn in the focused-border colour (`render::focused_border`, cyan):
/// on the frame that is the only thing that says which pane has the keys.
fn cyan(s: &vt100::Screen, row: u16, col: u16) -> bool {
    s.cell(row, col)
        .is_some_and(|c| c.fgcolor() == vt100::Color::Idx(6))
}

/// The help overlay's box as `(top border row, bottom border row)`, found by its title and
/// then by the column that title starts in, so the app's own frame cannot answer for it.
fn help_box_of(s: &vt100::Screen) -> Option<(usize, usize)> {
    let (_, cols) = s.size();
    let rows: Vec<String> = s.rows(0, cols).collect();
    let top = rows.iter().position(|r| r.contains("┌ keys "))?;
    let x = rows[top].chars().position(|c| c == '┌')?;
    let bottom = rows
        .iter()
        .enumerate()
        .skip(top + 1)
        .find(|(_, r)| r.chars().nth(x) == Some('└'))?
        .0;
    Some((top, bottom))
}

/// `?` through the terminal, and verifier (b) F4/F5 on a real frame: **any** key closes the
/// overlay (`q` included — inside it that is not the quit key), the way out is on the frame
/// at the exact-fit height as well as one row below it, and at 80×24 the clip notice names
/// the width that would show everything while all three footer rows survive.
///
/// The exact fit is measured rather than written down, so it moves with the keymap: at a
/// height that holds the whole box the box is `rows + 4` tall (`render::render_help`), and
/// the exact fit is one row less — the height at which the body fills the box and the draw
/// used to spend the footer's row on a key. Every resize waits on a marker only the **new**
/// frame can satisfy: at the exact fit the box's top border is on row 0 (at 30 rows it is
/// not), and one row below that the clip notice appears.
#[test]
fn pty_help_overlay_says_how_to_leave_and_any_key_closes() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);

    pty.send(b"?").expect("?");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("any key closes")
    })
    .unwrap_or_else(|e| panic!("the help overlay: {e}"));

    // 100 columns is the width two columns need and 30 rows hold them: every row of the
    // keymap is on the frame, the modal keys included, and nothing is clipped.
    let text = pty.screen_text();
    for row in [
        "page up",
        "page down",
        "toggle focus",
        "previous hunk",
        "expand a collapsed file",
        "full paths",
        "show org/repo",
        "clear the file's flags",
        "workspace scope on/off",
        "rescan now",
        "this help",
        "quit",
        "confirm",
        "cancel",
    ] {
        assert!(text.contains(row), "the overlay lists `{row}`:\n{text}");
    }
    assert!(!text.contains("more key"), "nothing is clipped:\n{text}");
    let (top, bottom) = pty
        .screen(help_box_of)
        .unwrap_or_else(|| panic!("the overlay's box:\n{text}"));
    let natural = (bottom - top + 1) as u16;
    assert!(top > 0, "at 30 rows the box is centred, not flush:\n{text}");

    // The exact fit: one row less than the box's natural height. The body fills the box and
    // the way out is still the last thing in it.
    let exact = natural - 1;
    pty.resize(100, exact).expect("resize");
    pty.wait_for(Duration::from_secs(5), |s| {
        help_box_of(s).is_some_and(|(top, _)| top == 0)
    })
    .unwrap_or_else(|e| panic!("the box fills a {exact}-row frame: {e}"));
    let rows = pty.rows();
    assert!(
        !rows.iter().any(|r| r.contains("more key")),
        "the exact fit shows every key:\n{}",
        pty.screen_text()
    );
    let (_, bottom) = pty.screen(help_box_of).expect("the overlay's box");
    assert!(
        rows[bottom - 1].contains("any key closes"),
        "the last inner row says how to leave:\n{}",
        pty.screen_text()
    );
    note(&format!(
        "PTY help: natural box {natural} rows, exact fit {exact}"
    ));

    // One row shorter it clips — which is what makes the height above it the exact fit —
    // and the clip is spent on keys, never on the way out.
    pty.resize(100, exact - 1).expect("resize");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("more key")
    })
    .unwrap_or_else(|e| panic!("one row shorter clips: {e}"));
    assert!(
        pty.screen_text().contains("any key closes"),
        "the clip drops keys, never the way out:\n{}",
        pty.screen_text()
    );

    // 80×24: too narrow for two columns and too short for one. The notice names the width
    // that would show everything, it sits directly above the pinned `quit`, and all three
    // footer rows are on the frame (ruling R12, verifier (b) F4).
    pty.resize(80, 24).expect("resize");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("columns shows all")
    })
    .unwrap_or_else(|e| panic!("the 80-column clip notice: {e}"));
    let rows = pty.rows();
    let at = rows
        .iter()
        .position(|r| r.contains("more key"))
        .expect("the clip notice");
    assert!(rows[at].contains("100 columns shows all"), "{}", rows[at]);
    assert!(
        rows[at + 1].contains("quit"),
        "the notice sits directly above the pinned quit:\n{}",
        pty.screen_text()
    );
    for footer in [
        "^J is a newline in the note",
        "shift+drag selects text",
        "any key closes",
    ] {
        assert!(
            rows.iter().any(|r| r.contains(footer)),
            "the footer row `{footer}` is on the 80×24 frame:\n{}",
            pty.screen_text()
        );
    }

    // A key that means something elsewhere closes the overlay and is **spent** on it: `j`
    // does not also move the selection (`app_help_opens_and_any_key_closes_it`). The next
    // `j` does, which is how the scene knows the loop kept the keys rather than the overlay.
    pty.resize(100, 30).expect("resize");
    pty.wait_for(Duration::from_secs(5), |s| {
        help_box_of(s).is_some_and(|(top, _)| top > 0)
    })
    .unwrap_or_else(|e| panic!("the overlay is centred again at 30 rows: {e}"));
    pty.send(b"j").expect("j closes the overlay");
    pty.wait_for(Duration::from_secs(5), |s| {
        !s.contents().contains("any key closes") && rows_listed(s)
    })
    .unwrap_or_else(|e| panic!("a key closes the overlay: {e}"));
    assert!(
        pty.screen(nav_cursor).is_none(),
        "and selects nothing: the key was spent on the overlay:\n{}",
        pty.screen_text()
    );
    pty.send(b"j").expect("j");
    pty.wait_for(Duration::from_secs(5), |s| {
        nav_cursor(s).as_deref() == Some("alpha")
    })
    .unwrap_or_else(|e| panic!("the loop is still taking keys: {e}"));

    // `q` is the one key the overlay does not merely swallow: it is the way out of lastcall
    // from inside the help, which is why the clip pins its row (`render_help`'s `pin_quit`)
    // and why `any key closes` is not a promise that `q` closes only the overlay.
    pty.send(b"?").expect("? again");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("any key closes")
    })
    .unwrap_or_else(|e| panic!("the overlay reopens: {e}"));
    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty
        .wait_exit(QUIT_BUDGET)
        .expect("exits after q from the overlay");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// `nav_page_up` / `nav_page_down` (both bindings each), `focus_toggle` and `hunk_prev`
/// (both bindings) through the terminal.
///
/// The page is read twice: at 100×30, where a page is longer than the whole nav and the
/// move is the clamp at either end, and at 100×14, where `App::page_rows` is ten and the
/// page lands in the middle of the fifteen entries the six extra files make — the scene
/// then **counts** the `j` presses back to the top, which is what tells a page from a jump
/// to the end and from a single step, without writing down which entry it should be.
#[test]
fn pty_page_keys_focus_toggle_and_hunk_prev() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let alpha = fx.repo("alpha");
    // Two separated hunks in f1 for `p`, and six added files so the nav is longer than a
    // page at the height below (nine entries otherwise, and a page of ten would clamp).
    alpha.write("f1", F1_TWO_HUNKS);
    for name in ADDED {
        alpha.write(name, format!("{name}\n"));
    }
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    pty.wait_for(LONG, |s| s.contents().contains("A g06"))
        .unwrap_or_else(|e| panic!("the six added rows: {e}"));

    // (1) the clamp, both ends and both bindings.
    pty.send(b"j").expect("j");
    pty.wait_for(Duration::from_secs(5), |s| {
        nav_cursor(s).as_deref() == Some("alpha")
    })
    .unwrap_or_else(|e| panic!("`j` selects the first entry: {e}"));
    let last = |s: &vt100::Screen| nav_cursor(s).is_some_and(|r| r.starts_with("M n2.md"));
    let first = |s: &vt100::Screen| nav_cursor(s).as_deref() == Some("alpha");
    for (keys, name, down) in [
        (&b" "[..], "Space", true),
        (&b"b"[..], "b", false),
        (&b"\x1b[6~"[..], "PgDn", true),
        (&b"\x1b[5~"[..], "PgUp", false),
    ] {
        pty.send(keys).expect("a page key");
        let reached = if down {
            pty.wait_for(Duration::from_secs(5), last)
        } else {
            pty.wait_for(Duration::from_secs(5), first)
        };
        reached.unwrap_or_else(|e| {
            panic!(
                "{name} reaches the {} entry: {e}\n{}",
                if down { "last" } else { "first" },
                pty.screen_text()
            )
        });
    }

    // (2) a page is `App::page_rows` entries — the frame's height less its four chrome
    // rows — not the whole list. At 14 rows that is ten, and the nav has fifteen entries.
    pty.resize(100, 14).expect("resize");
    pty.wait_for(Duration::from_secs(5), |s| {
        let (_, cols) = s.size();
        s.rows(0, cols).nth(12).is_some_and(|r| r.starts_with('└'))
    })
    .unwrap_or_else(|e| panic!("the 14-row frame: {e}"));
    assert!(
        pty.screen(first),
        "the resize kept the cursor on the first entry:\n{}",
        pty.screen_text()
    );
    pty.send(b"\x1b[6~").expect("PgDn");
    pty.wait_for(Duration::from_secs(5), |s| !first(s))
        .unwrap_or_else(|e| panic!("the page moves the cursor: {e}"));
    let landed = pty.screen(nav_cursor).expect("a nav cursor");
    assert!(
        !landed.starts_with("M n2.md"),
        "a page is not a jump to the end: it landed on `{landed}`\n{}",
        pty.screen_text()
    );
    pty.send(b"\x1b[5~").expect("PgUp");
    pty.wait_for(Duration::from_secs(5), first)
        .unwrap_or_else(|e| panic!("the page back reaches the first entry: {e}"));
    let mut steps = 0;
    loop {
        let here = pty.screen(nav_cursor).expect("a nav cursor");
        if here == landed {
            break;
        }
        assert!(steps < 20, "never walked to `{landed}` (at `{here}`)");
        pty.send(b"j").expect("j");
        pty.wait_for(Duration::from_secs(5), |s| {
            nav_cursor(s).as_ref() != Some(&here)
        })
        .unwrap_or_else(|e| panic!("`j` moves off `{here}`: {e}"));
        steps += 1;
    }
    assert_eq!(
        steps, 10,
        "one page down at 14 rows is `page_rows` = 14 − 4 entries (landed on `{landed}`)"
    );
    note(&format!(
        "PTY page: {steps} entries at 14 rows, on `{landed}`"
    ));
    pty.resize(100, 30).expect("resize");
    pty.wait_for(Duration::from_secs(5), |s| {
        let (_, cols) = s.size();
        s.rows(0, cols).nth(12).is_some_and(|r| r.starts_with('│'))
    })
    .unwrap_or_else(|e| panic!("the 30-row frame is back: {e}"));

    // (3) `n` then `p`, and `n` then `[`: the inverted hunk header walks forward and back.
    select_until(&mut pty, "f1  M  +2 −2");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().matches("@@ -").count() == 2
    })
    .unwrap_or_else(|e| panic!("f1's two hunks: {e}"));
    let headers = hunk_headers(&pty);
    assert_eq!(headers.len(), 2, "{headers:?}");
    let one = header_text(&headers[0].1);
    let two = header_text(&headers[1].1);
    assert_ne!(one, two);
    let current = |s: &vt100::Screen, want: &str| {
        let (_, cols) = s.size();
        s.rows(0, cols).enumerate().any(|(i, r)| {
            r.contains(want)
                && col_of(&r, "@@ -")
                    .and_then(|c| s.cell(i as u16, c))
                    .is_some_and(vt100::Cell::inverse)
        })
    };
    assert!(
        pty.screen(|s| current(s, &one)),
        "hunk 1 is current before `n`"
    );
    for (back, name) in [(&b"p"[..], "p"), (&b"["[..], "[")] {
        pty.send(b"n").expect("n");
        pty.wait_for(Duration::from_secs(5), |s| current(s, &two))
            .unwrap_or_else(|e| panic!("`n` moves to hunk 2 (before `{name}`): {e}"));
        pty.send(back).expect("hunk_prev");
        pty.wait_for(Duration::from_secs(5), |s| current(s, &one))
            .unwrap_or_else(|e| panic!("`{name}` walks back to hunk 1: {e}"));
    }

    // (4) `Tab`: the focused pane is the one with the coloured border, the hint line says
    // what the arrows now do, and while the diff has the keys `j` leaves the nav cursor
    // where it was.
    let (_, cols) = pty.screen(|s| s.size());
    let right = cols - 1;
    let nav_focused = move |s: &vt100::Screen| cyan(s, 1, 0) && !cyan(s, 1, right);
    let diff_focused = move |s: &vt100::Screen| cyan(s, 1, right) && !cyan(s, 1, 0);
    pty.wait_for(Duration::from_secs(5), nav_focused)
        .unwrap_or_else(|e| panic!("the nav's border is the coloured one: {e}"));
    let on_f1 = pty.screen(nav_cursor).expect("a nav cursor");
    assert!(on_f1.starts_with("M f1"), "{on_f1}");
    let rows_before = hunk_headers(&pty);
    pty.send(b"\t").expect("tab");
    pty.wait_for(Duration::from_secs(5), diff_focused)
        .unwrap_or_else(|e| panic!("`Tab` moves the focus to the diff: {e}"));
    pty.send(b"j").expect("j in the diff");
    pty.send(b"\t").expect("tab back");
    pty.wait_for(Duration::from_secs(5), nav_focused)
        .unwrap_or_else(|e| panic!("`Tab` is its own inverse: {e}"));
    // The frame that answered the second `Tab` has certainly answered the `j` before it: it
    // scrolled the diff by a line and left the nav cursor alone.
    // (hunk 1's header starts on the pane's first content row, so the line it loses is that
    // header itself: what is left is hunk 2's, one row higher than it was.)
    assert_eq!(
        hunk_headers(&pty).last().map(|(r, _)| *r),
        rows_before.last().map(|(r, _)| r - 1),
        "the `j` in the diff scrolled it by one line:\n{}",
        pty.screen_text()
    );
    assert_eq!(
        pty.screen(nav_cursor).as_deref(),
        Some(on_f1.as_str()),
        "and left the nav cursor where it was"
    );
    pty.send(b"j").expect("j in the nav");
    pty.wait_for(Duration::from_secs(5), |s| {
        nav_cursor(s).as_deref() != Some(on_f1.as_str())
    })
    .unwrap_or_else(|e| panic!("the nav has the keys back: {e}"));

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// `f` (full paths), `o` (the remote) and `e` (expand a collapsed row) through the terminal,
/// each its own inverse where it has one.
#[test]
fn pty_full_paths_remote_and_expand() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let alpha = fx.repo("alpha");
    // A lockfile: collapsed by the default `collapsed_globs`, so its diff view is a summary
    // and the `[e expand]` control rather than hunks. Twelve lines fit the pane once they
    // are asked for, so the expansion can be read to its last line.
    let lock: String = (1..=12)
        .map(|i| format!("\"pkg-{i}\" = \"1.0.{i}\"\n"))
        .collect();
    alpha.write("Cargo.lock", &lock);
    alpha
        .git(&[
            "remote",
            "set-url",
            "origin",
            "git@github.com:acme/alpha.git",
        ])
        .expect("set-url");

    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    pty.wait_for(LONG, |s| s.contents().contains("A Cargo.lock"))
        .unwrap_or_else(|e| panic!("the lockfile row: {e}"));

    // (1) `f`: the nav rows carry the path inside the repo, not the basename.
    assert!(
        pty.screen_text().contains("M parse.rs"),
        "the basename to begin with:\n{}",
        pty.screen_text()
    );
    pty.send(b"f").expect("f");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("M src/parse.rs")
    })
    .unwrap_or_else(|e| panic!("`f` shows the full path: {e}"));
    pty.send(b"f").expect("f back");
    pty.wait_for(Duration::from_secs(5), |s| {
        let t = s.contents();
        t.contains("M parse.rs") && !t.contains("M src/parse.rs")
    })
    .unwrap_or_else(|e| panic!("`f` is its own inverse: {e}"));

    // (2) `o`: the repo row gains `org/repo`, read from the remote's URL.
    pty.send(b"o").expect("o");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("alpha  acme/alpha")
    })
    .unwrap_or_else(|e| panic!("`o` shows the remote: {e}"));
    pty.send(b"o").expect("o back");
    pty.wait_for(Duration::from_secs(5), |s| {
        !s.contents().contains("acme/alpha")
    })
    .unwrap_or_else(|e| panic!("`o` is its own inverse: {e}"));

    // (3) `e`: a collapsed row's hunks are computed on demand and drawn under the summary,
    // which keeps its place.
    select_until(&mut pty, "Cargo.lock  A  +12 −0");
    pty.wait_for(Duration::from_secs(5), |s| {
        let t = s.contents();
        t.contains("collapsed (glob) · +12 −0") && t.contains("[e expand]")
    })
    .unwrap_or_else(|e| panic!("the collapsed summary: {e}\n{}", pty.screen_text()));
    assert!(
        !pty.screen_text().contains("@@ -"),
        "a collapsed row carries no hunks until it is asked:\n{}",
        pty.screen_text()
    );
    let t = Instant::now();
    pty.send(b"e").expect("e");
    pty.wait_for(OVERLOADED, |s| {
        let t = s.contents();
        t.contains("@@ -0,0 +1,12 @@") && t.contains("+\"pkg-12\" = \"1.0.12\"")
    })
    .unwrap_or_else(|e| panic!("`e` expands the row: {e}\n{}", pty.screen_text()));
    note(&format!(
        "PTY expand: hunks on the frame after {:.3?}",
        t.elapsed()
    ));
    assert!(
        pty.screen_text().contains("collapsed (glob) · +12 −0"),
        "the summary stays above the expansion:\n{}",
        pty.screen_text()
    );

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// `r` through the terminal: the rescan is asked for by hand and what it found is on the
/// frame, with `refreshed` on the status line to say the scan is over.
///
/// Both backstops are parked (`--poll 300`), so nothing else was going to look; where the
/// filesystem watcher delivers, the same row could also have arrived without the key (on
/// the development Mac, where fseventsd reports nothing under these temp dirs, it could
/// not). `refreshing…` is the in-flight half of the pair and is deliberately **not** waited
/// for here: over a three-root fixture the scan can be over before a poll of the screen
/// sees it, and a wait that sometimes passes for the wrong reason is worse than none. The
/// unit tier pins that half (`run.rs::run_local_results_feed_the_app`).
#[test]
fn pty_refresh_rescans_on_r() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let Ok(mut pty) = fx.command(&bin()).args(["tui", "--poll", "300"]).spawn() else {
        note("SKIP: this host cannot open a pty");
        return;
    };
    pty.wait_for(LONG, rows_listed)
        .unwrap_or_else(|e| panic!("first piles: {e}"));
    wait_watching(&mut pty);

    fx.append("alpha/f1", "appended between the scans\n");
    let t = Instant::now();
    pty.send(b"r").expect("r");
    pty.wait_for(OVERLOADED, |s| status_is(s, "refreshed"))
        .unwrap_or_else(|e| panic!("the refresh says it finished: {e}"));
    note(&format!("PTY refresh: refreshed in {:.3?}", t.elapsed()));
    pty.wait_for(OVERLOADED, |s| s.contents().contains("M f1  +2 −1"))
        .unwrap_or_else(|e| panic!("the rescan's own row: {e}\n{}", pty.screen_text()));

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// `tui --poll` earning its keep: a **new root** under the watched parent reaches the nav
/// with no key pressed and nothing a filesystem event could carry.
///
/// The engine watches roots, not the parent they sit in, and `Engine::scan_all` re-runs
/// discovery only for repos nested inside a root it already has — so a sibling checkout can
/// arrive by exactly one route, the `rescan` backstop `--poll` moves. This is the fallback
/// the docs promise where the watcher does not deliver, measured through the binary.
#[test]
fn pty_poll_finds_a_root_the_watcher_cannot_see() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    pty.wait_for_text("3 repos · ", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("three roots to begin with: {e}"));
    let before = pty.raw().len();

    FixtureRepo::new_in(TempDir::adopt(&fx.parent), "gamma").expect("a fourth checkout");
    let t = Instant::now();
    pty.wait_for(LONG, |s| {
        let text = s.contents();
        text.contains("4 repos · ") && text.contains("gamma")
    })
    .unwrap_or_else(|e| panic!("the poll finds the new root: {e}\n{}", pty.screen_text()));
    note(&format!(
        "PTY poll: a new root on the nav after {:.3?}",
        t.elapsed()
    ));
    let raw = pty.raw();
    assert!(
        find(&raw[..before], b"gamma").is_none(),
        "and not before it existed"
    );

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// The other half of the `ctrl-a` confirm (`y` is `pty_accept_loop_and_restart`): above ten
/// files the modal asks, and `n` closes it having accepted nothing — not on the frame, and
/// not in the ledger the next process would load. `Esc` is the same answer by the other
/// binding, and the modal really closed rather than merely stopped being drawn: `ctrl-a`
/// opens it again in between.
#[test]
fn pty_accept_all_confirm_n_accepts_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let alpha = fx.repo("alpha");
    // Six added files takes the total to twelve, above `CONFIRM_ABOVE`.
    for name in ADDED {
        alpha.write(name, format!("{name}\n"));
    }
    let before = fx.ledger("alpha");

    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    pty.wait_for(LONG, |s| {
        let t = s.contents();
        t.contains("A g06") && t.contains("3 repos · 12 files")
    })
    .unwrap_or_else(|e| panic!("all twelve rows: {e}\n{}", pty.screen_text()));

    for (key, name) in [(&b"n"[..], "n"), (&b"\x1b"[..], "Esc")] {
        pty.send(b"\x01").expect("ctrl-a");
        pty.wait_for(Duration::from_secs(5), |s| {
            s.contents().contains("Accept all 12 files across 3 repos?")
        })
        .unwrap_or_else(|e| panic!("the confirm modal (before `{name}`): {e}"));
        pty.send(key).expect("cancel");
        pty.wait_for(Duration::from_secs(5), |s| {
            !s.contents().contains("Accept all 12 files")
        })
        .unwrap_or_else(|e| panic!("`{name}` closes the modal: {e}"));
        // Everything is still pending: the rows, the header's counts, and the ledger.
        let text = pty.screen_text();
        assert!(text.contains("3 repos · 12 files"), "{text}");
        for row in ["M f1", "M f2", "A g01", "A g06", "M n2.md"] {
            assert!(text.contains(row), "{row} is still pending:\n{text}");
        }
        assert_eq!(fx.ledger("alpha"), before, "no ledger write after `{name}`");
    }

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// The nav divider is dragged with the mouse and has no key at all: press on it, move with
/// the button held, release. The nav follows the pointer, stops at `NAV_WIDTH_MAX`, and
/// holds its width once the button is up.
#[test]
fn pty_nav_divider_drag_widens_the_nav() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    let start = pty.screen(divider_col).expect("the divider");
    assert_eq!(start, 27, "`NAV_WIDTH_DEFAULT` (28) less its own column");

    // Press on the divider and move right: the nav widens to the pointer, live.
    let row = 10;
    pty.press(start, row).expect("press the divider");
    pty.drag_to(start + 12, row).expect("drag right");
    pty.wait_for(Duration::from_secs(5), |s| {
        divider_col(s) == Some(start + 12)
    })
    .unwrap_or_else(|e| panic!("the nav follows the pointer: {e}\n{}", pty.screen_text()));

    // Past `NAV_WIDTH_MAX` (60) it stops rather than eating the diff pane.
    pty.drag_to(90, row).expect("drag past the maximum");
    pty.wait_for(Duration::from_secs(5), |s| divider_col(s) == Some(59))
        .unwrap_or_else(|e| panic!("the nav stops at its maximum: {e}"));
    pty.release(90, row).expect("release");

    // Once the button is up a motion is nobody's business: the divider stays where the drag
    // left it. The `j` after it is the marker — a frame that answered the key has certainly
    // seen the motion that preceded it.
    pty.drag_to(20, row).expect("a motion after the release");
    pty.send(b"j").expect("j");
    pty.wait_for(Duration::from_secs(5), |s| {
        nav_cursor(s).as_deref() == Some("alpha")
    })
    .unwrap_or_else(|e| panic!("the key after the motion is answered: {e}"));
    assert_eq!(
        pty.screen(divider_col),
        Some(59),
        "the release ended the drag:\n{}",
        pty.screen_text()
    );

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// [`herdr_snapshot`] with the bare shell beside the agent made an agent too, and the
/// workspace given a short label: two candidates under one root, which is what opens the
/// picker instead of staging to the only agent there is.
fn herdr_snapshot_two_agents(root: &Path) -> serde_json::Value {
    let mut v = herdr_snapshot(root);
    v["snapshot"]["workspaces"][0]["label"] = serde_json::json!("ws1");
    let second = v["snapshot"]["panes"]
        .as_array_mut()
        .expect("the snapshot's panes")
        .iter_mut()
        .find(|p| p["pane_id"] == serde_json::json!("w1:p2"))
        .expect("the bare shell beside the agent");
    second["agent"] = serde_json::json!("codex");
    second["agent_status"] = serde_json::json!("ready");
    v
}

/// `w` and the agent picker in one herdr session (§6.6, deliverable 10).
///
/// The mock puts both of the workspace's panes in `alpha` and the child is started with
/// `HERDR_WORKSPACE_ID`, so the scope is `{alpha}` and the other two repos are hidden: the
/// header counts one repo, the hint line says so, `w` shows all three and `w` hides them
/// again. Then a flag on a file in `alpha` has two agents it could go to, so the picker
/// asks which — and `Esc` drops the send while the flag itself stays on disk.
#[test]
fn pty_herdr_scope_toggle_and_the_agent_picker() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let alpha = std::fs::canonicalize(fx.parent.join("alpha")).expect("alpha exists");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime for the mock");
    let sock = fx.state.join("herdr.sock");
    let mock = rt.block_on(async {
        MockHerdr::builder()
            .snapshot(herdr_snapshot_two_agents(&alpha))
            .canned(
                "notification.show",
                serde_json::json!({"type": "notification_shown", "shown": true, "reason": ""}),
            )
            .serve(&sock)
            .await
            .expect("bind the mock socket")
    });

    let Ok(mut pty) = fx
        .command(&bin())
        .args(["tui", "--poll", "1"])
        .env("HERDR_SOCKET_PATH", &sock)
        .env("HERDR_WORKSPACE_ID", "w1")
        .spawn()
    else {
        note("SKIP: this host cannot open a pty");
        return;
    };
    let t = pty
        .wait_for(LONG, |s| {
            let text = s.contents();
            text.contains("1 repo · ") && text.contains("M f1") && !text.contains("beta")
        })
        .unwrap_or_else(|e| panic!("the workspace scope: {e}\n{}", pty.screen_text()));
    note(&format!("PTY scope: alpha alone after {t:.3?}"));
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents()
            .contains("scope: ws1 · 2 repos hidden (w shows all)")
    })
    .unwrap_or_else(|e| panic!("the scope notice: {e}\n{}", pty.screen_text()));

    pty.send(b"w").expect("w");
    pty.wait_for(Duration::from_secs(5), |s| {
        let text = s.contents();
        text.contains("3 repos · ") && text.contains("beta") && text.contains("notes")
    })
    .unwrap_or_else(|e| panic!("`w` shows every repo: {e}\n{}", pty.screen_text()));
    pty.send(b"w").expect("w back");
    pty.wait_for(Duration::from_secs(5), |s| {
        let text = s.contents();
        text.contains("1 repo · ") && !text.contains("beta")
    })
    .unwrap_or_else(|e| panic!("`w` is its own inverse: {e}\n{}", pty.screen_text()));

    // The picker: two agents in this root, so the flag asks where it should go.
    select_until(&mut pty, "f1  M  +1 −1");
    pty.send(b"m").expect("m");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("f1 · whole file")
    })
    .unwrap_or_else(|e| panic!("the note modal: {e}\n{}", pty.screen_text()));
    pty.send("which of you wrote this?".as_bytes())
        .expect("the note");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("which of you wrote this?")
    })
    .unwrap_or_else(|e| panic!("the note is echoed: {e}"));
    pty.send(b"\r").expect("enter");
    pty.wait_for(OVERLOADED, |s| {
        let text = s.contents();
        text.contains("send to")
            && text.contains("demo · ws1")
            && text.contains("codex · ws1")
            && text.contains("Esc cancel")
    })
    .unwrap_or_else(|e| panic!("the agent picker: {e}\n{}", pty.screen_text()));

    // (`status_is` cannot be used here: under a scope the bottom row carries the mandatory
    // notice on its right, so the status does not own the line.)
    pty.send(b"\x1b").expect("esc");
    pty.wait_for(Duration::from_secs(5), |s| {
        let (_, cols) = s.size();
        s.rows(0, cols)
            .last()
            .is_some_and(|r| r.starts_with("flagged f1 · not sent · "))
    })
    .unwrap_or_else(|e| panic!("`Esc` drops the send: {e}\n{}", pty.screen_text()));
    assert!(
        !pty.screen_text().contains("send to"),
        "the picker is gone:\n{}",
        pty.screen_text()
    );
    // The flag itself was written before the picker ever opened, and nothing was staged.
    pty.wait_for_text("M f1 ⚑", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("the nav flag marker: {e}"));
    let ledger = fx.ledger("alpha");
    assert_eq!(
        ledger["overrides"]["f1"]["flags"].as_array().map(Vec::len),
        Some(1),
        "{ledger}"
    );
    assert_eq!(
        mock.control().count("pane.send_text"),
        0,
        "a cancelled picker sends nothing: {:?}",
        mock.control().methods()
    );

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
    rt.block_on(mock.shutdown());
}

// ---- Phase 10: undo and snooze (Amendment v1.11) -----------------------------------------

/// How deep the root's undo stack is on disk, from `ledger.json`.
fn undo_depth(fx: &Fixture, name: &str) -> usize {
    fx.ledger(name)["undo"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0)
}

/// Deliverable 2 through the real binary: `A` accepts a file, `z` puts it back — the row
/// returns to the nav with the cursor on it, and the ledger on disk is what a second
/// process would load, entry and all.
#[test]
fn pty_undo_file() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    assert_eq!(undo_depth(&fx, "notes"), 0, "nothing accepted yet");

    // notes has exactly one pending file.
    select_until(&mut pty, "n2.md  M ");
    pty.send(b"A").expect("A");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "accepted n2.md") && s.contents().contains("nothing pending in notes")
    })
    .unwrap_or_else(|e| panic!("the accept: {e}\n{}", pty.screen_text()));
    assert_eq!(undo_depth(&fx, "notes"), 1, "one entry on disk");

    // `z` puts it back: the status names the file, the row returns, and the header counts
    // it again.
    let t = Instant::now();
    pty.send(b"z").expect("z");
    pty.wait_for(OVERLOADED, |s| {
        let text = s.contents();
        status_is(s, "undid accept of n2.md")
            && text.contains("M n2.md")
            && text.contains("3 repos · 6 files")
    })
    .unwrap_or_else(|e| panic!("the undo: {e}\n{}", pty.screen_text()));
    note(&format!(
        "PTY undo: the row is back after {:.3?}",
        t.elapsed()
    ));

    let text = pty.screen_text();
    let row = pty
        .find_row(|r| r.contains("M n2.md"))
        .unwrap_or_else(|| panic!("the restored row:\n{text}"));
    assert!(
        pty.inverse_at(row, 1),
        "the cursor is on the file that came back:\n{text}"
    );
    assert_eq!(undo_depth(&fx, "notes"), 0, "the entry was spent");

    // A second `z` has nothing left, and says so rather than reaching further back.
    pty.send(b"z").expect("z again");
    pty.wait_for(Duration::from_secs(5), |s| {
        status_is(s, "nothing to undo in notes")
    })
    .unwrap_or_else(|e| panic!("the empty stack: {e}\n{}", pty.screen_text()));

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// Deliverable 2, the sweep: `ctrl-a` writes one entry per root, so `z` in one of them
/// puts that repo back and says how many others are still accepted. The sentence is the
/// whole point — without it `z` after a sweep reads as an undo of the sweep.
#[test]
fn pty_undo_accept_all() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);

    // Six files across three repos is under the confirm threshold, so `^A` folds at once.
    pty.send(b"\x01").expect("ctrl-a");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "accepted 6 files in 3 repos")
            && s.contents().contains("3 repos · 0 files · 0 hunks")
    })
    .unwrap_or_else(|e| panic!("the sweep: {e}\n{}", pty.screen_text()));
    for name in ["alpha", "beta", "notes"] {
        assert_eq!(undo_depth(&fx, name), 1, "{name} has its own entry");
    }

    // `z` on alpha: alpha's three files come back, the other two repos do not. The pane's
    // own sentence is the unambiguous marker — with every repo empty the *unselected* pane
    // lists all three names, so a walk that stopped at `alpha  main` would never move.
    select_until(&mut pty, "nothing pending in alpha");
    pty.send(b"z").expect("z");
    pty.wait_for(OVERLOADED, |s| {
        status_is(
            s,
            "undid accept of 3 files in alpha (2 other repos have their own undo)",
        )
    })
    .unwrap_or_else(|e| panic!("the sweep-aware sentence: {e}\n{}", pty.screen_text()));
    pty.wait_for(Duration::from_secs(5), |s| {
        let text = s.contents();
        text.contains("3 repos · 3 files") && text.contains("M f1") && !text.contains("M n2.md")
    })
    .unwrap_or_else(|e| panic!("only alpha came back: {e}\n{}", pty.screen_text()));
    assert_eq!(undo_depth(&fx, "alpha"), 0);
    assert_eq!(undo_depth(&fx, "beta"), 1, "beta is still accepted");
    assert_eq!(undo_depth(&fx, "notes"), 1, "and so is notes");

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

/// Deliverable 3 through the real binary: `s` on a repository row asks how long, Enter
/// writes the deadline, the repository leaves the nav with a notice saying how to get it
/// back, `S` shows it, and `s` on a shown one wakes it. The deadline is on disk, so the
/// next process starts where this one left off.
#[test]
fn pty_snooze_repo() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    assert!(fx.ledger("beta")["snoozed_until"].is_null(), "awake");

    // (1) `s` on beta's repo row opens the modal, seeded with one day.
    select_until(&mut pty, "beta  main · 2 files");
    pty.send(b"s").expect("s");
    pty.wait_for(Duration::from_secs(5), |s| {
        let text = s.contents();
        text.contains("snooze beta for [1") && text.contains("Esc cancel")
    })
    .unwrap_or_else(|e| panic!("the snooze modal: {e}\n{}", pty.screen_text()));

    // Esc costs nothing.
    pty.send(b"\x1b").expect("esc");
    pty.wait_for(Duration::from_secs(5), |s| !s.contents().contains("day(s)"))
        .unwrap_or_else(|e| panic!("Esc closes it: {e}\n{}", pty.screen_text()));
    assert!(fx.ledger("beta")["snoozed_until"].is_null(), "still awake");

    // (2) `s` again, `3`, Enter: three days, written and off the nav.
    pty.send(b"s").expect("s");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("snooze beta for [1")
    })
    .unwrap_or_else(|e| panic!("the modal again: {e}\n{}", pty.screen_text()));
    pty.send(b"\x7f3").expect("backspace then 3");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("snooze beta for [3")
    })
    .unwrap_or_else(|e| panic!("the field takes digits: {e}\n{}", pty.screen_text()));
    let t = Instant::now();
    pty.send(b"\r").expect("enter");
    pty.wait_for(OVERLOADED, |s| {
        let text = s.contents();
        text.contains("snoozed beta until 20")
            && text.contains("1 snoozed (S shows)")
            && !text.contains("A u1")
            && text.contains("2 repos · 4 files")
    })
    .unwrap_or_else(|e| panic!("beta leaves the nav: {e}\n{}", pty.screen_text()));
    note(&format!(
        "PTY snooze: off the nav after {:.3?}",
        t.elapsed()
    ));
    let deadline = fx.ledger("beta")["snoozed_until"]
        .as_str()
        .unwrap_or_else(|| panic!("a deadline on disk: {}", fx.ledger("beta")))
        .to_owned();
    assert!(
        deadline.ends_with('Z') && deadline.len() == 20,
        "{deadline}"
    );

    // (3) `S` shows it again, and says so on its branch line. The nav pane is 26 columns
    // wide, so what fits there is the word; the date itself is pinned at a nav width that
    // holds it by `render_nav_says_when_a_repo_is_snoozed`.
    pty.send(b"S").expect("shift-s");
    pty.wait_for(Duration::from_secs(5), |s| {
        let text = s.contents();
        text.contains("3 repos · 6 files") && text.contains("A u1") && text.contains("· snoozed")
    })
    .unwrap_or_else(|e| panic!("`S` shows it: {e}\n{}", pty.screen_text()));

    // (4) and `s` on a shown one wakes it, with no question to ask.
    select_until(&mut pty, "beta  main · 2 files");
    pty.send(b"s").expect("s");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "woke beta") && !s.contents().contains("snoozed until")
    })
    .unwrap_or_else(|e| panic!("the wake: {e}\n{}", pty.screen_text()));
    assert!(
        fx.ledger("beta")["snoozed_until"].is_null(),
        "the deadline is off the ledger: {}",
        fx.ledger("beta")
    );

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
}

// ---- the first-launch welcome (Amendment v1.11, deliverable 1) --------------------------

/// `lastcall::tui::tour::MARKER_FILE`, and `pty_tui::MARKER_FILE` beside it.
const MARKER_FILE: &str = "first-launch.json";
/// `config::write::CREATED_BY`.
const CREATED_BY: &str = "# written by lastcall's first-launch tour on ";

/// A herdr mock over `sock` whose workspace is both panes in `alpha`: the link the herdr
/// card's condition needs, and the scope it is about.
fn mock_in_alpha(
    rt: &tokio::runtime::Runtime,
    fx: &Fixture,
    sock: &Path,
) -> lastcall_testkit::mock_herdr::MockHerdr {
    let alpha = std::fs::canonicalize(fx.parent.join("alpha")).expect("alpha exists");
    rt.block_on(async {
        MockHerdr::builder()
            .snapshot(herdr_snapshot(&alpha))
            .serve(sock)
            .await
            .expect("bind the mock socket")
    })
}

/// Wait for the welcome's first card.
fn wait_welcome(pty: &mut PtyTui) {
    pty.wait_for(LONG, |s| {
        let text = s.contents();
        text.contains("welcome") && text.contains("Welcome to lastcall")
    })
    .unwrap_or_else(|e| panic!("the welcome: {e}\n{}", pty.screen_text()));
}

/// Deliverable 1 through the real binary, on a machine with **no configuration file at
/// all**: the welcome opens over the live screen, `enter` walks to the herdr card, and the
/// second row writes the one key it says it writes. The file that appears is the whole
/// evidence — a provenance comment and the line the reader chose, and nothing else.
#[test]
fn tui_tour_first_launch() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let xdg = fx.state.join("xdg");
    let config = xdg.join("lastcall").join("config.toml");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime for the mock");
    let sock = fx.state.join("herdr.sock");
    let mock = mock_in_alpha(&rt, &fx, &sock);

    let Ok(mut pty) = fx
        .command(&bin())
        .no_config_file(&xdg)
        .tour(true)
        .args(["tui", "--poll", "1"])
        .env("HERDR_SOCKET_PATH", &sock)
        .env("HERDR_WORKSPACE_ID", "w1")
        .spawn()
    else {
        note("SKIP: this host cannot open a pty");
        return;
    };
    assert!(!config.exists(), "no configuration file to start with");
    wait_welcome(&mut pty);
    // The screen underneath is live: the welcome is an overlay, not a splash.
    assert!(
        pty.screen_text().contains("lastcall  1 repo"),
        "the header is still there:\n{}",
        pty.screen_text()
    );

    // Card one is the keys; `enter` goes to card two, which is about the workspace.
    pty.send(b"\r").expect("enter");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("You are running inside herdr 0.8.2")
    })
    .unwrap_or_else(|e| panic!("the herdr card: {e}\n{}", pty.screen_text()));

    // The second row is the one that writes; it is not the selected one.
    pty.send(b"\x1b[B").expect("down");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("> Show every repository instead")
    })
    .unwrap_or_else(|e| panic!("the second row: {e}\n{}", pty.screen_text()));
    pty.send(b"\r").expect("enter");

    // It was the last card, so the welcome closes; and the choice took effect at once —
    // every repository is listed, which is what it promised.
    pty.wait_for(OVERLOADED, |s| {
        let text = s.contents();
        !text.contains("Welcome to lastcall") && text.contains("beta")
    })
    .unwrap_or_else(|e| panic!("the choice takes hold: {e}\n{}", pty.screen_text()));

    let written = std::fs::read_to_string(&config).expect("the tour wrote the config file");
    note(&format!(
        "PTY tour: the config file it created, in full:\n{written}"
    ));
    let mut lines = written.lines();
    let comment = lines.next().expect("a provenance comment");
    assert!(
        comment.starts_with(CREATED_BY) && comment.len() == CREATED_BY.len() + "2026-09-14".len(),
        "the comment names the day it was written: {comment:?}"
    );
    assert_eq!(
        lines.collect::<Vec<_>>(),
        vec!["[herdr]", "scope = \"all\""],
        "one key, and nothing else, in {written:?}"
    );
    assert!(
        fx.state.join(MARKER_FILE).exists(),
        "and the welcome recorded that it has been seen"
    );

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    let status = pty.wait_exit(QUIT_BUDGET).expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
    rt.block_on(mock.shutdown());
}

/// The file is the user's. The one write the tour is allowed to make is format-preserving:
/// every comment, every blank line and every key the tour did not set comes back byte for
/// byte, and the file grows by exactly the lines the card named.
#[test]
fn tui_tour_preserves_config() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime for the mock");
    let sock = fx.state.join("herdr.sock");
    let mock = mock_in_alpha(&rt, &fx, &sock);

    // A hand-written file, with the shape a hand-written file has: a comment at the top, a
    // blank line, a commented `[herdr]` table with a key of its own, and the fixture's
    // `[update]` table last (the harness rewrites `check` in place inside it).
    std::fs::write(
        &fx.config,
        format!(
            "# my lastcall\n\
             parent_dirs = [\"{}\"]\n\
             draft_dirs = [\"_drafts\"]\n\
             \n\
             # the agents live next door\n\
             [herdr]\n\
             # no desktop notifications, thanks\n\
             toast = false\n\
             \n\
             [update]\n\
             check = false\n",
            fx.parent.display()
        ),
    )
    .expect("a hand-written config");

    let Ok(mut pty) = fx
        .command(&bin())
        .tour(true)
        .args(["tui", "--poll", "1"])
        .env("HERDR_SOCKET_PATH", &sock)
        .env("HERDR_WORKSPACE_ID", "w1")
        .spawn()
    else {
        note("SKIP: this host cannot open a pty");
        return;
    };
    // Read it back after the spawn: the harness appends `[update]` at spawn time, so this
    // is the file the child actually opened.
    let before = std::fs::read_to_string(&fx.config).expect("the config the child reads");
    wait_welcome(&mut pty);
    pty.send(b"\r").expect("enter");
    pty.wait_for(Duration::from_secs(5), |s| {
        s.contents().contains("You are running inside herdr 0.8.2")
    })
    .unwrap_or_else(|e| panic!("the herdr card: {e}\n{}", pty.screen_text()));
    pty.send(b"\x1b[B").expect("down");
    pty.send(b"\r").expect("enter");
    pty.wait_for(OVERLOADED, |s| {
        let text = s.contents();
        !text.contains("Welcome to lastcall") && text.contains("beta")
    })
    .unwrap_or_else(|e| panic!("the choice takes hold: {e}\n{}", pty.screen_text()));

    let after = std::fs::read_to_string(&fx.config).expect("the config after the write");
    for line in before.lines() {
        assert!(
            after.lines().any(|l| l == line),
            "the write dropped {line:?} from\n{after}"
        );
    }
    assert!(after.contains("# my lastcall"), "{after}");
    assert!(after.contains("# the agents live next door"), "{after}");
    assert!(
        after.contains("# no desktop notifications, thanks"),
        "a comment inside the table the write touched: {after}"
    );
    let added: Vec<&str> = after
        .lines()
        .filter(|l| !before.lines().any(|b| b == *l))
        .collect();
    assert_eq!(
        added,
        vec!["scope = \"all\""],
        "one line added to a file that already had a [herdr] table:\n{after}"
    );
    assert!(
        !after.contains(CREATED_BY),
        "a file that already existed gets no provenance comment:\n{after}"
    );

    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    assert_eq!(pty.wait_exit(QUIT_BUDGET).expect("exit").exit_code(), 0);
    assert_clean_exit(&pty, since);
    rt.block_on(mock.shutdown());
}

/// `q` on the first card skips the rest: the keys come back at once, nothing is written to
/// the config file, and the marker says it has been seen — skipping is an answer.
#[test]
fn tui_tour_skip() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let Some(mut pty) = ({
        let cmd = fx.command(&bin()).tour(true).args(["tui", "--poll", "1"]);
        match cmd.spawn() {
            Ok(p) => Some(p),
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => None,
            Err(e) => panic!("spawn lastcall tui: {e}"),
        }
    }) else {
        note("SKIP: this host cannot open a pty");
        return;
    };
    let before = std::fs::read_to_string(&fx.config).expect("the config the child reads");
    wait_welcome(&mut pty);
    // `?` while the welcome is up is not the help overlay: its own keys are the only keys,
    // and the card is still the card a second later.
    pty.send(b"?").expect("?");
    assert!(
        pty.wait_for(Duration::from_secs(1), |s| !s
            .contents()
            .contains("Welcome to lastcall"))
            .is_err(),
        "a keymap key reached the screen under the welcome:\n{}",
        pty.screen_text()
    );

    pty.send(b"q").expect("q skips");
    pty.wait_for(Duration::from_secs(5), |s| {
        let text = s.contents();
        !text.contains("Welcome to lastcall") && text.contains("M f1")
    })
    .unwrap_or_else(|e| panic!("the review screen comes back: {e}\n{}", pty.screen_text()));
    assert!(!pty.eof(), "`q` skipped the welcome, it did not quit");

    // And the keys are the keys again.
    pty.send(b"?").expect("? for real");
    pty.wait_for(Duration::from_secs(5), |s| s.contents().contains("quit"))
        .unwrap_or_else(|e| panic!("the help overlay: {e}\n{}", pty.screen_text()));
    pty.send(b"\x1b").expect("esc");

    assert_eq!(
        std::fs::read_to_string(&fx.config).expect("the config"),
        before,
        "skipping writes nothing to the config file"
    );
    assert!(
        fx.state.join(MARKER_FILE).exists(),
        "skipping is an answer: it is not asked again"
    );

    let since = pty.raw().len();
    pty.send(b"q").expect("q quits");
    assert_eq!(pty.wait_exit(QUIT_BUDGET).expect("exit").exit_code(), 0);
    assert_clean_exit(&pty, since);
}

/// The marker is the whole decision: a second launch shows the review screen, and
/// `lastcall tui --tour` shows the welcome again over it.
#[test]
fn tui_tour_flag() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    // A marker is already there (the harness writes one into every isolated state dir):
    // no welcome, straight to the rows.
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    wait_first_piles(&mut pty);
    assert!(
        !pty.screen_text().contains("Welcome to lastcall"),
        "a state dir that has seen it is not asked again:\n{}",
        pty.screen_text()
    );
    let since = pty.raw().len();
    pty.send(b"q").expect("q");
    assert_eq!(pty.wait_exit(QUIT_BUDGET).expect("exit").exit_code(), 0);
    assert_clean_exit(&pty, since);

    // `--tour` over the very same state dir, marker and all.
    let Ok(mut pty) = fx
        .command(&bin())
        .args(["tui", "--poll", "1", "--tour"])
        .spawn()
    else {
        note("SKIP: this host cannot open a pty");
        return;
    };
    wait_welcome(&mut pty);
    let since = pty.raw().len();
    pty.send(b"q").expect("q skips");
    pty.wait_for(Duration::from_secs(5), |s| {
        !s.contents().contains("Welcome to lastcall")
    })
    .unwrap_or_else(|e| panic!("the skip: {e}\n{}", pty.screen_text()));
    pty.send(b"q").expect("q quits");
    assert_eq!(pty.wait_exit(QUIT_BUDGET).expect("exit").exit_code(), 0);
    assert_clean_exit(&pty, since);
}

/// Quitting with the welcome still open is a dismissal too: ctrl-c leaves, and the marker
/// it wrote on the way out means the next launch starts on the review screen.
#[test]
fn tui_tour_quit_writes_marker() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::build();
    let marker = fx.state.join(MARKER_FILE);
    let Some(mut pty) = ({
        let cmd = fx.command(&bin()).tour(true).args(["tui", "--poll", "1"]);
        match cmd.spawn() {
            Ok(p) => Some(p),
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => None,
            Err(e) => panic!("spawn lastcall tui: {e}"),
        }
    }) else {
        note("SKIP: this host cannot open a pty");
        return;
    };
    assert!(!marker.exists(), "the welcome is owed");
    wait_welcome(&mut pty);

    let since = pty.raw().len();
    pty.send(b"\x03").expect("ctrl-c");
    let status = pty
        .wait_exit(QUIT_BUDGET)
        .expect("ctrl-c leaves from inside the welcome");
    assert_eq!(status.exit_code(), 0, "{status:?}");
    assert_clean_exit(&pty, since);
    // The marker the child wrote, not the harness's: it names the build that showed it, so
    // the next launch of this state directory starts on the review screen.
    let marker = std::fs::read_to_string(&marker).expect("the marker was written on the way out");
    assert!(
        marker.contains(&format!("\"version\":\"{}\"", env!("CARGO_PKG_VERSION"))),
        "the binary's own version: {marker}"
    );
    assert!(marker.contains("\"shown_at\":"), "{marker}");
}
