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

/// `f1` with line 1 and line 10 changed against the seen `a1..a10`: two separated hunks.
const F1_TWO_HUNKS: &str = "A1\na2\na3\na4\na5\na6\na7\na8\na9\nA10\n";
const F2_EDIT: &str = "b\nagent edit\nmore\n";
const F2_EDIT_AGAIN: &str = "b\nagent edit\nmore\nagain\n";
const F3_EDIT: &str = "c changed\n";
/// Added files so that after the hunk and file accepts `ctrl-a` still covers eleven files
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
        t.contains("M f3") && t.contains("A g06") && t.contains("3 repos · 12 files")
    })
    .unwrap_or_else(|e| panic!("all twelve rows: {e}"));
    note(&format!(
        "PTY accept loop: 12 files on screen after {first:.3?}"
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

    // (3) `ctrl-a`: eleven files across three roots is above the confirm threshold; the
    // modal counts u1 as the one grouped upstream row; `y` folds all three ledgers.
    pty.send(b"\x01").expect("ctrl-a");
    pty.wait_for(Duration::from_secs(5), |s| {
        let text = s.contents();
        text.contains("Accept all 11 files across 3 repos?")
            && text.contains("1 grouped upstream · 0 collapsed")
    })
    .unwrap_or_else(|e| panic!("confirm modal: {e}"));
    let t = Instant::now();
    pty.send(b"y").expect("y");
    pty.wait_for(OVERLOADED, |s| {
        status_is(s, "accepted 11 files in 3 repos")
            && s.contents().contains("nothing pending across 3 roots")
    })
    .unwrap_or_else(|e| panic!("accept all: {e}"));
    note(&format!(
        "PTY accept all: nothing pending after {:.3?}",
        t.elapsed()
    ));
    pty.wait_for_text("0 repos · 0 files · 0 hunks", Duration::from_secs(5))
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
    // agent's commit has it, the six added files.
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

    // (5) a second process on the same state dir: the empty state, after scanning (the
    // `watching <parent> (3 roots)` status replaces `scanning 3 roots…` once the
    // post-install rescans are done; the temp path is long, so only its head fits).
    let Some(mut pty) = fx.spawn_tui(&bin()) else {
        return;
    };
    let t = Instant::now();
    pty.wait_for(LONG, |s| {
        let (_, cols) = s.size();
        s.rows(0, cols)
            .last()
            .is_some_and(|r| r.starts_with("watching "))
    })
    .unwrap_or_else(|e| panic!("relaunch scanned: {e}"));
    assert!(
        find_words(&pty.raw(), &["scanning", "3", "roots…"]).is_some(),
        "the relaunch scanned first"
    );
    note(&format!("PTY relaunch: scanned after {:.3?}", t.elapsed()));
    let text = pty.screen_text();
    assert!(text.contains("nothing pending across 3 roots"), "{text}");
    assert!(text.contains("0 repos · 0 files · 0 hunks"), "{text}");
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
    pty.wait_for_text("1 repo · 1 file · 1 hunk", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("header after the re-edit: {e}"));
    note(&format!("PTY relaunch edit-to-screen {took:.3?}"));
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
    pty.wait_for_text("scanning", LONG)
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
