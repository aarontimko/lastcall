//! Bare `lastcall` and `lastcall tui [--poll <secs>]`: the Ratatui app (kickoff
//! deliverable 1). Everything that can fail loudly happens here, before the terminal is
//! touched: stdout not a TTY (exit 2, never draws into a pipe), a `[keys]` table that does
//! not parse (exit 2, the message names the action or spec), an engine that cannot open
//! (exit 1, like `status` and `watch`). `--poll` shortens both polling backstops exactly as
//! for `watch`. Exit 0 on `q`, Ctrl-C or SIGTERM.

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use lastcall::tui::herdr::HerdrPlan;
use lastcall::tui::input::Keymap;
use lastcall::tui::{run, term};
use lastcall_engine::config;
use lastcall_engine::engine::Engine;
use lastcall_engine::env::Env;

/// The message for a piped stdout; printed to stderr, exit 2.
pub const NOT_A_TERMINAL: &str = "lastcall: not a terminal; try `lastcall status`";

/// What [`discovering`] prefixes; the whole line is on **stderr**, because stdout is the
/// terminal the alternate screen is about to take.
pub const DISCOVERING: &str = "lastcall: discovering roots under ";

/// The one line the user sees while `Engine::open` walks the parent dirs (Phase 6
/// deliverable 7). Discovery over a large tree is seconds of nothing, and a terminal that
/// prints nothing at all reads as a hang; the alternate screen then replaces this line, so
/// it costs the finished screen nothing. `None` when there is nothing to name — a config
/// that resolved no parent dir has an error to print, not a progress line.
fn discovering(parent_dirs: &[std::path::PathBuf]) -> Option<String> {
    let named: Vec<String> = parent_dirs
        .iter()
        .map(|d| d.display().to_string())
        .collect();
    (!named.is_empty()).then(|| format!("{DISCOVERING}{}…", named.join(", ")))
}

pub fn run(poll: Option<u64>) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if !io::stdout().is_terminal() {
        eprintln!("{NOT_A_TERMINAL}");
        return Ok(ExitCode::from(2));
    }
    let env = Env::from_process();
    let loaded = config::load(&env)?;
    let keymap = match Keymap::from_config(&loaded.config.keys) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("lastcall: {e}");
            return Ok(ExitCode::from(2));
        }
    };
    let resolved = loaded.resolve(env.cwd());
    // File-only tracing (`LASTCALL_LOG_FILE`), or nothing: the screen is about to be ours.
    // Installed **before** the open so the engine's own `open done roots= ms=` line is
    // loggable — that is the line a slow discovery is diagnosed with.
    let _ = term::init_tracing();
    if let Some(line) = discovering(&resolved.parent_dirs) {
        eprintln!("{line}");
    }
    let engine = match Engine::open(&loaded, &resolved, &env, crate::commands::engine_options()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("lastcall: {e}");
            return Ok(ExitCode::from(1));
        }
    };
    // What `[herdr]` asks for, resolved before the terminal is taken; the link itself is
    // opened inside the loop's runtime, after the first frame (kickoff deliverable 4).
    let plan = HerdrPlan::of(&loaded.config.herdr, &env);
    run::run(engine, super::poll_timings(poll), keymap, env, plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn tui_discovering_line_names_every_parent_dir() {
        assert_eq!(discovering(&[]), None, "nothing to name, nothing to print");
        assert_eq!(
            discovering(&[PathBuf::from("/W")]).as_deref(),
            Some("lastcall: discovering roots under /W…")
        );
        assert_eq!(
            discovering(&[PathBuf::from("/W"), PathBuf::from("/home/a/src")]).as_deref(),
            Some("lastcall: discovering roots under /W, /home/a/src…")
        );
    }
}
