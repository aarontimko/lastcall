//! Terminal lifecycle (kickoff deliverable 2).
//!
//! `enter()` installs a panic hook *before* touching the terminal, then enables raw mode,
//! the alternate screen and mouse capture (bracketed paste stays off). `restore()` undoes all
//! of it, is idempotent, and is what the panic hook, `Drop` and the run loop's shutdown
//! path share, so a crash never leaves the user's shell in raw mode.
//!
//! Logging never goes to stdout or stderr while the alternate screen is up: `init_tracing`
//! writes only to `LASTCALL_LOG_FILE` (filtered by `LASTCALL_LOG`, default `info`), and is a
//! no-op when that variable is unset. These reads and [`KEYBOARD_ENV`] are the only
//! environment access in the TUI; everything else comes through the engine's `Env`.
//!
//! **Keyboard enhancement** (Phase 8 deliverable 5, ruling P9). `enter()` asks the terminal
//! once per process whether it speaks the kitty keyboard protocol and, when it does, pushes
//! `DISAMBIGUATE_ESCAPE_CODES` so `Shift-Enter` arrives as something other than `Enter` —
//! the one key the note modal can only promise under that protocol. The probe writes
//! `ESC [ ? u ESC [ c` and waits up to 2 s for a reply, which is a long time to spend on a
//! terminal that will never answer, so `LASTCALL_KEYBOARD=plain` skips it: the test harness
//! sets that, and so can anyone whose terminal is slow to answer. The answer is cached, so
//! a `$EDITOR` suspend and resume pays for it once. `restore()` pops the flags **before**
//! leaving the alternate screen, on every exit path — quit, panic and suspend alike.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Once, OnceLock};

use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};

/// Set while the terminal is in our raw/alternate-screen state.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Set while our keyboard-enhancement flags are on the terminal's stack.
static PUSHED: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();
/// The probe's answer, taken once per process (ruling P9).
static ENHANCED: OnceLock<bool> = OnceLock::new();

/// Set this to `plain` to skip the keyboard-enhancement probe entirely.
///
/// An environment switch and **not** a `[config]` key: §6.1 is frozen, and this is a
/// property of the terminal a session happens to be running in rather than of the user's
/// preferences.
pub const KEYBOARD_ENV: &str = "LASTCALL_KEYBOARD";

/// Owns the terminal state; dropping it calls [`restore`].
#[derive(Debug)]
pub struct TerminalGuard {
    _private: (),
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Install the panic hook (once), then enter raw mode + alternate screen + mouse capture.
///
/// Fails with `ErrorKind::Unsupported` when stdout is not a terminal, before changing
/// anything; the caller prints the one permitted message and exits.
pub fn enter() -> io::Result<TerminalGuard> {
    if !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "stdout is not a terminal",
        ));
    }
    install_panic_hook();
    // Mark active first so a failure half-way is still undone by `restore`.
    ACTIVE.store(true, Ordering::SeqCst);
    let result = (|| {
        enable_raw_mode()?;
        // The probe before the alternate screen and before `spawn_input`: it reads the
        // terminal's reply through crossterm's own internal reader, which the input thread
        // would otherwise be racing for, and any byte a confused terminal echoes lands on
        // the normal screen the alternate screen is about to cover.
        let enhanced = keyboard_enhanced();
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        if enhanced {
            execute!(
                io::stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
            PUSHED.store(true, Ordering::SeqCst);
        }
        Ok::<(), io::Error>(())
    })();
    if let Err(e) = result {
        restore();
        return Err(e);
    }
    Ok(TerminalGuard { _private: () })
}

/// Leave the alternate screen, drop mouse capture and raw mode. Safe to call any number of
/// times, from the panic hook, from `Drop`, or from a signal path; only the first call after
/// [`enter`] does work. Uses nothing but crossterm commands on stdout.
pub fn restore() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = io::stdout();
    // Errors are ignored on purpose: this runs while unwinding and there is no better place
    // to report them; the disables are independent so each is attempted.
    // `Show` too: ratatui hides the cursor on every draw and only the normal quit path
    // calls `Terminal::show_cursor`; the panic and `Drop` paths come through here alone.
    // The keyboard flags come off **first**: they are the terminal's state, not the
    // alternate screen's, and popping them after the screen is gone leaves the user's shell
    // reading `Shift-Enter` as an escape sequence.
    if PUSHED.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableMouseCapture, DisableBracketedPaste, Show);
    let _ = execute!(out, LeaveAlternateScreen);
    let _ = disable_raw_mode();
    let _ = out.flush();
}

/// True between a successful [`enter`] and the matching [`restore`].
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

/// Whether this terminal speaks the kitty keyboard protocol — asked once, then remembered.
///
/// Called from [`enter`], so a `$EDITOR` suspend and resume re-pushes the flags without
/// re-asking (F8, F18). Callers outside the terminal lifecycle — the note modal's key
/// mapping, its hint line — read the cached answer through `App`.
pub fn keyboard_enhanced() -> bool {
    *ENHANCED.get_or_init(|| enhancement_answer(plain_requested(), supports_keyboard_enhancement))
}

/// `LASTCALL_KEYBOARD=plain`, and only that spelling: an unset variable, an empty one or
/// any other value means "ask the terminal".
fn plain_requested() -> bool {
    std::env::var_os(KEYBOARD_ENV).is_some_and(|v| v == "plain")
}

/// The rule, apart from the terminal: `plain` wins without asking, and a probe that errors
/// or times out is a "no".
///
/// Failing to "off" is the whole point. Enhancement buys exactly one extra key
/// (`Shift-Enter`), and every terminal already has `Ctrl-J` for it; guessing "on" from a
/// failed probe would push flags a terminal never agreed to and change how it reports keys
/// the app depends on.
fn enhancement_answer(plain: bool, probe: impl FnOnce() -> io::Result<bool>) -> bool {
    if plain {
        return false;
    }
    probe().unwrap_or(false)
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            original(info);
        }));
    });
}

/// Route `tracing` to `LASTCALL_LOG_FILE` (appended), filtered by `LASTCALL_LOG` (an
/// `EnvFilter` directive, default `info`). Returns the file path when logging is on, `None`
/// when `LASTCALL_LOG_FILE` is unset or the file cannot be opened. Never writes to the
/// terminal. A second call (or a subscriber set elsewhere) is a harmless no-op.
pub fn init_tracing() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("LASTCALL_LOG_FILE")?);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    let filter = std::env::var("LASTCALL_LOG")
        .ok()
        .and_then(|spec| tracing_subscriber::EnvFilter::try_new(spec).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .try_init();
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_restore_is_idempotent_when_never_entered() {
        // Nothing was entered, so restore must be a no-op (and not touch the terminal).
        assert!(!is_active());
        restore();
        restore();
        assert!(!is_active());
    }

    /// Ruling P9: the switch short-circuits the probe, an error is "off", and only a
    /// terminal that says yes gets the flags pushed.
    #[test]
    fn term_keyboard_enhancement_fails_open_to_plain() {
        let boom = || Err(io::Error::other("no reply"));
        assert!(!enhancement_answer(true, || Ok(true)), "plain never asks");
        assert!(!enhancement_answer(false, boom), "an error is off");
        assert!(!enhancement_answer(false, || Ok(false)));
        assert!(enhancement_answer(false, || Ok(true)));
        // A `plain` answer is never derived from the probe's own timing, so the switch is
        // the only thing the harness needs to set to keep a scene off the 2 s wait.
        assert!(!enhancement_answer(true, || panic!(
            "the probe must not run"
        )));
    }

    #[test]
    fn term_enter_refuses_a_non_terminal_stdout() {
        // Under `cargo test` stdout is captured, never a tty: `enter` must refuse before it
        // changes any terminal state.
        if io::stdout().is_terminal() {
            return; // running with --nocapture on a real tty: nothing to assert safely
        }
        let err = enter().expect_err("captured stdout is not a terminal");
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(!is_active());
    }
}
