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
//! `probe_tui_screen` (ignored) is `just probe-tui-screen`: the same flow against the
//! release binary, printing the final screen and the exit code.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lastcall_testkit::fixture_parent;
use lastcall_testkit::pty_tui::{PtyCommand, PtyTui, col_of, vt100};
use lastcall_testkit::tmp::TempDir;

/// `commands/tui.rs::NOT_A_TERMINAL` (the binary crate's private module; kept in sync by
/// `pty_non_tty_stdout_exits_2_without_drawing`).
const NOT_A_TERMINAL: &str = "lastcall: not a terminal; try `lastcall status`";
/// `render::TOO_SMALL`.
const TOO_SMALL: &str = "too small: 40×10 min";

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

/// Where `words` first appear in order in the transcript (the offset of the first). A
/// frame's text is not contiguous in the raw bytes: ratatui skips cells equal to the
/// previous buffer, so every blank between two words of a fresh frame becomes a cursor
/// move.
fn find_words(hay: &[u8], words: &[&str]) -> Option<usize> {
    let mut at = 0;
    let mut first = None;
    for word in words {
        let i = at + find(&hay[at..], word.as_bytes())?;
        first.get_or_insert(i);
        at = i + word.len();
    }
    first
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
        match self.command(bin).args(["tui", "--poll", "1"]).spawn() {
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
/// the empty state with the `scanning` status: in the raw transcript that text precedes
/// the first file row.
fn wait_first_piles(pty: &mut PtyTui) -> Duration {
    let took = pty
        .wait_for(LONG, rows_listed)
        .unwrap_or_else(|e| panic!("first piles: {e}"));
    let raw = pty.raw();
    let scanning =
        find_words(&raw, &["scanning", "3", "roots…"]).expect("first frame: scanning status");
    let first_row = find(&raw, b"f1").expect("a file row");
    assert!(
        scanning < first_row,
        "the scanning status ({scanning}) precedes the first row ({first_row})"
    );
    assert!(
        find_words(&raw, &["nothing", "pending", "across", "3", "roots"])
            .is_some_and(|i| i < first_row),
        "the first frame is the empty state"
    );
    assert!(
        find(&raw, ALT_SCREEN_ON).is_some() && find(&raw, MOUSE_ON).is_some(),
        "alternate screen and mouse capture are on"
    );
    assert!(pty.screen(|s| s.alternate_screen()));
    assert!(pty.screen(|s| s.mouse_protocol_mode() != vt100::MouseProtocolMode::None));
    took
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
    // The header's totals are live too: 3 repos, 5 files, 6 hunks (f1 now has two).
    pty.wait_for_text("3 repos · 5 files · 6 hunks", Duration::from_secs(5))
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
