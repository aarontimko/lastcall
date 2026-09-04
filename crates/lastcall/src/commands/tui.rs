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
    // File-only tracing (`LASTCALL_LOG_FILE`), or nothing: the screen is about to be ours.
    let _ = term::init_tracing();
    run::run(engine, super::poll_timings(poll), keymap, env, plan)
}
