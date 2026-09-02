//! `lastcall` binary. Phase 1 ships `config` and `hello-herdr`; the Ratatui app arrives in
//! Phase 3.

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
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Config { json } => commands::config::run(json),
        Command::HelloHerdr { socket, exit_after } => {
            commands::hello_herdr::run(socket, exit_after)
        }
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("lastcall: {err}");
            std::process::ExitCode::from(2)
        }
    }
}
