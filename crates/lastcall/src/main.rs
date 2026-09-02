//! `lastcall` binary. Phase 1 ships `config` and `hello-herdr`; Phase 2 adds `status` and
//! `watch` over the headless engine; Phase 3 adds the Ratatui app: bare `lastcall` (no
//! subcommand) and its explicit spelling `lastcall tui [--poll <secs>]`.

mod commands;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "lastcall", version, about = "The last call before code ships.")]
struct Cli {
    /// With no subcommand, `lastcall` opens the TUI (`lastcall tui`).
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug, PartialEq)]
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
    /// Open the review screen (the default when no subcommand is given).
    Tui {
        /// Poll HEAD and rescan every N seconds instead of 10 s / 30 s: the backstop for
        /// hosts whose filesystem events are late or missing.
        #[arg(long, value_name = "SECS")]
        poll: Option<u64>,
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

impl Cli {
    /// The command to run: bare `lastcall` is `tui` with the default timings.
    fn command(self) -> Command {
        self.command.unwrap_or(Command::Tui { poll: None })
    }
}

fn main() -> std::process::ExitCode {
    let result = match Cli::parse().command() {
        Command::Config { json } => commands::config::run(json),
        Command::HelloHerdr { socket, exit_after } => {
            commands::hello_herdr::run(socket, exit_after)
        }
        Command::Status { json, roots } => commands::status::run(json, roots),
        Command::Tui { poll } => commands::tui::run(poll),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(std::iter::once("lastcall").chain(args.iter().copied()))
            .expect("parses")
            .command()
    }

    #[test]
    fn cli_bare_lastcall_is_the_tui_with_default_timings() {
        assert_eq!(parse(&[]), Command::Tui { poll: None });
        assert_eq!(parse(&["tui"]), Command::Tui { poll: None });
    }

    #[test]
    fn cli_tui_poll_reaches_the_timings_with_the_watch_clamp() {
        assert_eq!(
            parse(&["tui", "--poll", "0"]),
            Command::Tui { poll: Some(0) }
        );
        let Command::Tui { poll } = parse(&["tui", "--poll", "0"]) else {
            unreachable!()
        };
        let timings = commands::poll_timings(poll);
        assert_eq!(timings.head_poll, std::time::Duration::from_secs(1));
        assert_eq!(timings.rescan, std::time::Duration::from_secs(1));
        assert_eq!(
            parse(&["watch", "--poll", "2"]),
            Command::Watch {
                json: false,
                exit_after: None,
                poll: Some(2)
            }
        );
    }

    #[test]
    fn cli_other_subcommands_are_unchanged() {
        assert_eq!(parse(&["config", "--json"]), Command::Config { json: true });
        assert_eq!(
            parse(&["status"]),
            Command::Status {
                json: false,
                roots: vec![]
            }
        );
        assert!(Cli::try_parse_from(["lastcall", "nope"]).is_err());
        assert!(Cli::try_parse_from(["lastcall", "tui", "--json"]).is_err());
    }
}
