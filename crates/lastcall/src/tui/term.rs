//! Terminal lifecycle (kickoff deliverable 2).
//!
//! `enter()` installs a panic hook *before* touching the terminal, then enables raw mode,
//! the alternate screen and mouse capture (bracketed paste stays off). `restore()` undoes all
//! of it, is idempotent, and is what the panic hook, `Drop` and the run loop's shutdown
//! path share, so a crash never leaves the user's shell in raw mode.
//!
//! Logging never goes to stdout or stderr while the alternate screen is up: `init_tracing`
//! writes only to `LASTCALL_LOG_FILE` (filtered by `LASTCALL_LOG`, default `info`), and is a
//! no-op when that variable is unset. These two reads are the only environment access in the
//! TUI; everything else comes through the engine's `Env`.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Set while the terminal is in our raw/alternate-screen state.
static ACTIVE: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();

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
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
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
    let _ = execute!(out, DisableMouseCapture, DisableBracketedPaste);
    let _ = execute!(out, LeaveAlternateScreen);
    let _ = disable_raw_mode();
    let _ = out.flush();
}

/// True between a successful [`enter`] and the matching [`restore`].
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
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
