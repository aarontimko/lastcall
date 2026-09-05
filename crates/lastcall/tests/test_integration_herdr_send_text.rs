//! Phase 7 deliverable 6, the real-herdr half: `tui::herdr::stage` puts the export into a
//! live pane's **input buffer** and does not submit it.
//!
//! Why a real server and a real shell (F7): bracketed-paste markers are interpreted by the
//! *application* on the far side of the tty, never by the line discipline. A `cat` or a
//! non-interactive shell reads `\x1b[200~echo ONE\necho TWO\x1b[201~` as ordinary bytes and
//! runs both lines — so a mock, or a pane running anything but an interactive shell, would
//! prove the opposite of what we need. The pane here runs `zsh -f -i`, whose `zle`
//! `bracketed-paste` widget is on by default and holds a pasted multi-line payload in the
//! editing buffer until the human presses Enter — or, where there is no zsh on the host,
//! `bash --norc --noprofile -i` with readline's `enable-bracketed-paste` switched on (bash
//! 5.1+ has it on by default; the `bind` makes the premise explicit). GitHub's
//! `ubuntu-latest` image ships no zsh, and its `/bin/sh` is dash, which exits when an
//! `exec` fails — so the pane's shell was gone by the second `send_text` and herdr
//! answered `pane_not_found` (CI 2026-09-05). The shell is chosen by looking for `zsh` on
//! this process's `PATH`; the pane runs on the same host.
//!
//! The mock-transport unit test (`herdr_stage_wraps_the_export_in_bracketed_paste_markers`)
//! pins the exact request; this pins what the request *does*.
//!
//! Skips visibly without `LASTCALL_TEST_HERDR_BIN` (`just test-integration-herdr` sets it),
//! the same rule as the engine's real tier.

use std::io::Write;
use std::time::Duration;

use lastcall::tui::herdr::stage;
use lastcall_engine::herdr::transport::{SocketTransport, Transport};
use lastcall_testkit::herdr_spawn::{SpawnedHerdr, herdr_bin_from_env, write_skip_notice};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(5);
/// How long a screen state may take to settle before the assertion gives up.
const SETTLE: Duration = Duration::from_secs(20);
/// How long a *negative* claim waits before it is believed — the grace an unstaged payload
/// would need to execute and print. The positive control below runs well inside it.
const SETTLE_WINDOW: Duration = Duration::from_millis(1500);
/// A prompt nothing else on the screen can be mistaken for.
const PROMPT: &str = "LCPROMPT";

fn say(msg: &str) {
    let _ = std::io::stderr().write_all(format!("herdr-real send_text: {msg}\n").as_bytes());
}

/// The pane's visible screen, ANSI stripped.
async fn screen<T: Transport>(t: &T, pane_id: &str) -> String {
    let v: Value = t
        .request(
            "pane.read",
            json!({ "pane_id": pane_id, "source": "visible", "strip_ansi": true }),
        )
        .await
        .expect("pane.read");
    // The result nests the payload under `read` (`PaneReadResult`).
    v["read"]["text"].as_str().unwrap_or_default().to_owned()
}

/// Poll the pane until `want` holds, or fail with the last screen we saw.
async fn wait_for<T: Transport>(
    t: &T,
    pane_id: &str,
    what: &str,
    want: impl Fn(&str) -> bool,
) -> String {
    let deadline = tokio::time::Instant::now() + SETTLE;
    let mut last = String::new();
    while tokio::time::Instant::now() < deadline {
        last = screen(t, pane_id).await;
        if want(&last) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("timed out waiting for {what}; last screen was:\n{last}");
}

/// Type `text` into the pane exactly as given — no paste markers. Used only to get the
/// shell up; the thing under test is [`stage`].
async fn send_raw<T: Transport>(t: &T, pane_id: &str, text: &str) {
    t.request(
        "pane.send_text",
        json!({ "pane_id": pane_id, "text": text }),
    )
    .await
    .expect("pane.send_text");
}

/// The interactive shell for the pane: the `exec` line that replaces the pane's `/bin/sh`,
/// then the lines that make its prompt recognisable and its paste handling explicit.
fn interactive_shell() -> (&'static str, Vec<String>) {
    let prompt = format!("PS1='{PROMPT} '\r");
    let has_zsh = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("zsh").is_file()));
    if has_zsh {
        ("exec zsh -f -i\r", vec![prompt])
    } else {
        (
            "exec bash --norc --noprofile -i\r",
            vec!["bind 'set enable-bracketed-paste on'\r".to_owned(), prompt],
        )
    }
}

/// Lines that are exactly `word` — command *output*, as opposed to `echo ONE` sitting on a
/// prompt as pending input.
fn output_lines(screen: &str, word: &str) -> usize {
    screen.lines().filter(|l| l.trim() == word).count()
}

#[tokio::test]
async fn herdr_real_send_text_lands_unsubmitted() {
    let Some(bin) = herdr_bin_from_env() else {
        write_skip_notice();
        return;
    };
    let mut herdr = SpawnedHerdr::spawn(&bin).expect("spawn herdr in a PTY");
    herdr
        .wait_for_socket(Duration::from_secs(5))
        .expect("socket appears within 5 s");
    let sock = herdr.socket_path().to_path_buf();
    let t = SocketTransport::new(&sock, TIMEOUT);

    let cwd = herdr.isolation().base.clone();
    let created: Value = t
        .request(
            "workspace.create",
            json!({ "cwd": cwd.to_string_lossy(), "focus": true }),
        )
        .await
        .expect("workspace.create");
    let pane = created["root_pane"]["pane_id"]
        .as_str()
        .expect("root_pane.pane_id")
        .to_string();
    say(&format!("hosting pane {pane}"));

    // An interactive shell with no rc files, then a prompt we can recognise. `exec` replaces
    // the pane's own shell so nothing underneath can answer instead.
    let (exec_line, setup) = interactive_shell();
    say(&format!("shell: {}", exec_line.trim_end()));
    send_raw(&t, &pane, exec_line).await;
    for line in &setup {
        send_raw(&t, &pane, line).await;
    }
    // The prompt itself starts a line; the echoed commands that set things up carry quotes.
    wait_for(&t, &pane, "the shell prompt", |s| {
        s.lines()
            .any(|l| l.starts_with(PROMPT) && !l.contains('\''))
    })
    .await;
    say("interactive shell is up");

    // The gesture under test. The payload that reaches the shell is the two lines plus
    // `STAGE_TAIL` — a newline and a blank line, inside the markers — so what sits on the
    // prompt is a three-line buffer whose last line is empty; the pasted newlines are text to
    // a bracketed-paste line editor, and the assertions below are what say so.
    stage(&t, &pane, "echo ONE\necho TWO").await.expect("stage");
    wait_for(&t, &pane, "the staged text on the prompt", |s| {
        s.contains("echo ONE") && s.contains("echo TWO")
    })
    .await;
    // Nothing ran *yet* is a weaker claim than nothing ran: give the shell a settle window
    // it would need only if the paste were going to execute, then read the screen again.
    tokio::time::sleep(SETTLE_WINDOW).await;
    let staged = screen(&t, &pane).await;
    assert!(
        staged.contains("echo ONE") && staged.contains("echo TWO"),
        "the staged lines left the prompt:\n{staged}"
    );
    assert_eq!(
        output_lines(&staged, "ONE"),
        0,
        "a staged paste must not run — `ONE` appeared as output:\n{staged}"
    );
    assert_eq!(output_lines(&staged, "TWO"), 0, "{staged}");
    say("both lines sit on the prompt, unsubmitted, with the separator pasted after them");

    // ...and one Enter runs the whole buffer, once.
    send_raw(&t, &pane, "\r").await;
    let ran = wait_for(&t, &pane, "the output of both echoes", |s| {
        output_lines(s, "ONE") > 0 && output_lines(s, "TWO") > 0
    })
    .await;
    assert_eq!(output_lines(&ran, "ONE"), 1, "submitted once:\n{ran}");
    assert_eq!(output_lines(&ran, "TWO"), 1, "{ran}");
    say("Enter submitted the buffer once");

    // The control that makes the two assertions above mean something: the *same* payload
    // without the markers runs on its own, no Enter involved. The embedded newline is a
    // keystroke to the line editor, so `echo THREE` executes the moment it arrives (only
    // `echo FOUR`, which has no newline after it, is left on the prompt). The markers are
    // the whole feature, not decoration.
    send_raw(&t, &pane, "echo THREE\necho FOUR").await;
    // Both halves of the screen state are waited for, not just the first: under load the
    // `THREE` output can land a frame before the trailing `echo FOUR` is drawn, and asserting
    // on the screen that merely satisfied the first half is the race that made this flake.
    let bare = wait_for(&t, &pane, "the unmarked payload running by itself", |s| {
        output_lines(s, "THREE") > 0 && s.contains("echo FOUR")
    })
    .await;
    assert_eq!(output_lines(&bare, "THREE"), 1, "{bare}");
    say("without the markers the same text runs — the wrapping is what stages it");
}
