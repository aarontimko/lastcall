//! Phase 8 deliverable 7, the end no reducer test can reach: `shift-i` really **spawns a
//! program**, and the program really lands on the line the hunk names.
//!
//! Everything up to the spawn is already pinned elsewhere — `editor.rs`'s basename table is
//! a table of unit tests, `Hunk::editor_line()` has its own, and `App::edit_target` is a
//! reducer test. What none of them can see is the argv that reaches a real `execve`, the
//! working directory the child is given, or whether the terminal comes back afterwards. So
//! this scene drives the **built binary** inside a PTY, points `$EDITOR` at
//! `tests/probe/editor.sh` **through a symlink named `vim`** (which is what exercises the
//! basename table: lastcall must key the line flag off the link's name, not off the script's
//! contents), and reads what the script wrote down.
//!
//! Never a real editor: `PtyCommand::isolated_lastcall` removes `$VISUAL` and `$EDITOR` from
//! every child, so the only editor any test can reach is the one it points at itself, inside
//! its own temp dir.
//!
//! The row is `alpha/src/parse.rs` and the hunk is its **second** (design review F5, F9): the
//! first hunk of `f1` starts at line 1, where a wrong answer — `1`, or `new_range.start + 1`
//! three lines above the change — is indistinguishable from a right one. The expected line
//! is computed here from the fixture text (`fixture_parent::parse_rs_edit2_line`), never from
//! the engine that is under test.
//!
//! Not an e2e scene: it asserts one seam (the spawn) rather than a user's journey, and the
//! e2e tier already carries the two editor scenes that do (`pty_editor_save_pends_nothing`,
//! `pty_editor_ctrl_c_does_not_quit_lastcall`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lastcall_testkit::fixture_parent;
use lastcall_testkit::pty_tui::{PtyCommand, vt100};
use lastcall_testkit::tmp::TempDir;

/// The probe editor, by absolute path: a test binary's `CARGO_MANIFEST_DIR` is the package
/// root, so this does not depend on the working directory the tier is run from.
const PROBE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/probe/editor.sh");

/// Hard bound on any single wait; the fixture build dominates it.
const LONG: Duration = Duration::from_secs(30);

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lastcall"))
}

/// The status bar reads `<text> · <age>` for exactly `text` (`test_e2e_tui_pty`'s rule).
fn status_is(s: &vt100::Screen, text: &str) -> bool {
    let (_, cols) = s.size();
    s.rows(0, cols).last().is_some_and(|r| {
        r.trim_end()
            .strip_prefix(&format!("{text} · "))
            .is_some_and(|age| !age.is_empty() && !age.contains(' '))
    })
}

/// The `@@ -a,b +c,d @@` of the topmost hunk header on screen. The diff pane scrolls the
/// current hunk's header to its top, so this is how a test sees which hunk `n` moved to
/// without reading the app's state.
fn top_hunk_header(s: &vt100::Screen) -> Option<String> {
    let (_, cols) = s.size();
    let row = s.rows(0, cols).find(|r| r.contains("@@ -"))?;
    let start = row.find("@@ -")?;
    let rest = &row[start..];
    let end = rest[4..].find("@@")? + 4 + 2;
    Some(rest[..end].to_owned())
}

/// Poll `log` until it holds `lines` newline-terminated lines, then return them.
fn wait_for_log(log: &Path, lines: usize, timeout: Duration) -> Vec<String> {
    let start = Instant::now();
    loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        let got: Vec<String> = text.lines().map(str::to_owned).collect();
        if got.len() >= lines {
            return got;
        }
        assert!(
            start.elapsed() < timeout,
            "the probe editor wrote {} of {lines} lines within {timeout:?}: {got:?}",
            got.len()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `shift-i` on `alpha/src/parse.rs`'s second hunk spawns the probe editor with a `vim`
/// argv — `+<line> <absolute path>` — in the root's own directory, and the TUI comes back.
#[test]
fn editor_launch_lands_at_the_right_line() {
    let w = TempDir::new("lc-edit-w");
    let state = TempDir::new("lc-edit-state");
    let parent = w.join("W");
    let built =
        fixture_parent::build(&parent, state.path(), &state.join("home")).expect("fixture builds");
    let config = state.join("config.toml");
    fixture_parent::write_config(&config, &parent).expect("config written");

    // The symlink is what makes the basename table's `vim` row the one lastcall picks. It
    // lives in the test's own temp dir and `$EDITOR` names it by absolute path: nothing is
    // put on `PATH`, so no editor of the developer's can be reached from here.
    let bindir = state.mkdir("bin");
    let vim = bindir.join("vim");
    std::os::unix::fs::symlink(PROBE, &vim).expect("the `vim` symlink");
    let log = state.join("editor.log");

    let cmd = PtyCommand::new(bin())
        .cwd(&parent)
        .isolated_lastcall(&built.home, &config, state.path())
        .env("EDITOR", &vim)
        .env("LASTCALL_PROBE_EDITOR_LOG", &log)
        .args(["tui", "--poll", "1"]);
    let mut pty = match cmd.spawn() {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
            eprintln!("SKIP: this host cannot open a pty: {e}");
            return;
        }
        Err(e) => panic!("spawn lastcall tui: {e}"),
    };

    // alpha's nav is [root, f1, f2, src/parse.rs]: four `j`s, then `⏎` opens it and focuses
    // the diff — `edit_target` reads the hunk under the cursor only from the diff.
    pty.wait_for_text("M parse.rs  +10 −2", LONG)
        .unwrap_or_else(|e| panic!("the first piles list parse.rs: {e}"));
    // The watcher emits its `watching <parent> (3 roots)` notice once, and a notice is a
    // **status**: one that arrives while the editor has the terminal drains over the return
    // path's `no change` the moment the loop runs again. Waiting for it here is how this
    // scene says which status it is reading later.
    pty.wait_for(LONG, |s| {
        let (_, cols) = s.size();
        s.rows(0, cols)
            .last()
            .is_some_and(|r| r.starts_with("watching "))
    })
    .unwrap_or_else(|e| panic!("the watch is live: {e}"));
    pty.send(b"jjjj\r").expect("keys");
    pty.wait_for_text("parse.rs  M  +10 −2", LONG)
        .unwrap_or_else(|e| panic!("parse.rs opens in the diff pane: {e}"));
    let first = pty
        .screen(top_hunk_header)
        .expect("a hunk header on screen");
    assert!(
        first.starts_with("@@ -1,"),
        "hunk 1 is the module doc at the top of the file, not {first}"
    );

    // `n` to hunk 2 — the comment-skipping condition in the middle of the file, three lines
    // of context below its `@@`.
    pty.send(b"n").expect("n");
    pty.wait_for(LONG, |s| top_hunk_header(s).is_some_and(|h| h != first))
        .unwrap_or_else(|e| panic!("`n` scrolls hunk 2's header to the top: {e}"));

    pty.send(b"I").expect("shift-i");
    let logged = wait_for_log(&log, 2, LONG);

    let alpha = std::fs::canonicalize(parent.join("alpha")).expect("alpha exists");
    let file = alpha.join(fixture_parent::PARSE_RS);
    let line = fixture_parent::parse_rs_edit2_line();
    assert_eq!(
        logged,
        vec![
            format!("argv: +{line} {}", file.display()),
            format!("cwd: {}", alpha.display()),
        ],
        "the probe editor's argv and working directory"
    );

    // …and the TUI took the terminal back. The probe wrote nothing, so the return path's
    // answer is `no change` (deliverable 3) and the same row is still open.
    pty.wait_for(LONG, |s| status_is(s, "no change"))
        .unwrap_or_else(|e| panic!("the resume folds the return: {e}"));
    assert!(
        pty.screen(|s| s.alternate_screen()),
        "back on the alternate screen"
    );
    assert!(
        pty.screen_text().contains("parse.rs  M  +10 −2"),
        "the same row is still selected and open:\n{}",
        pty.screen_text()
    );

    // The resumed input thread still reads keys: `q` is the proof the fresh channel and the
    // replaced guard work, not just the redraw.
    pty.send(b"q").expect("q");
    let status = pty
        .wait_exit(Duration::from_secs(5))
        .expect("exits after q");
    assert_eq!(status.exit_code(), 0, "{status:?}");
}
