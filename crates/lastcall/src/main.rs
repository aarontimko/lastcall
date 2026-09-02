//! `lastcall` binary. Phase 1 ships `config` and `hello-herdr`; Phase 2 adds `status` and
//! `watch` over the headless engine; the Ratatui app arrives in Phase 3.

mod commands;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "lastcall", version, about = "The last call before code ships.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the effective resolved configuration and any notices.
    Config {
        /// Print as JSON instead of the human-readable form.
        #[arg(long)]
        json: bool,
    },
    /// Connect to a herdr session, print its state, and stream events until Ctrl-C.
    HelloHerdr {
        /// Socket path override (skips session discovery; also exits on disconnect).
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        /// End the stream cleanly after this many seconds (exit 0).
        #[arg(long)]
        exit_after: Option<u64>,
    },
    /// Scan every root once and print the pending set.
    Status {
        /// Print the stable `status_version: 1` JSON report.
        #[arg(long)]
        json: bool,
        /// Only report these roots (any directory inside a root); repeatable.
        #[arg(long = "root")]
        roots: Vec<std::path::PathBuf>,
    },
    /// Run the watcher and print one line per engine event until Ctrl-C.
    Watch {
        /// One JSON object per line.
        #[arg(long)]
        json: bool,
        /// Exit cleanly after this many seconds (exit 0).
        #[arg(long)]
        exit_after: Option<u64>,
        /// Poll HEAD and rescan every N seconds instead of 10 s / 30 s: the backstop for
        /// hosts whose filesystem events are late or missing.
        #[arg(long, value_name = "SECS")]
        poll: Option<u64>,
    },
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Config { json } => commands::config::run(json),
        Command::HelloHerdr { socket, exit_after } => {
            commands::hello_herdr::run(socket, exit_after)
        }
        Command::Status { json, roots } => commands::status::run(json, roots),
        Command::Watch {
            json,
            exit_after,
            poll,
        } => commands::watch::run(json, exit_after, poll),
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("lastcall: {err}");
            std::process::ExitCode::from(2)
        }
    }
}
