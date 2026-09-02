//! `lastcall hello-herdr [--socket <path>] [--exit-after <secs>]`
//!
//! Discover the session (docs/spec/00-spec.md §6.6), print the ping result and the compat
//! verdict, bootstrap, print a table of workspaces/panes/agents from the snapshot, then stream:
//! every lifecycle event (one line each), every status transition as
//! `[<pane_id>] <agent> working → done`, every resync (`resync: snapshot` /
//! `resync: pane.get <id>`), and connection state changes, until Ctrl-C.
//!
//! `--socket` overrides discovery (the built-artifact pass runs it against the mock) and makes
//! the command exit on `Disconnected` instead of reconnecting; `--exit-after` ends the stream
//! cleanly (exit 0) so non-interactive probes terminate.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use lastcall_engine::config::{self, HerdrMode};
use lastcall_engine::env::Env;
use lastcall_engine::herdr::client::{
    Cache, Client, ClientOptions, ClientTimings, HerdrEvent, ResyncTarget,
};
use lastcall_engine::herdr::discovery::{self, Discovery};
use lastcall_engine::herdr::transport::{SocketTransport, socket_answers_ping};
use lastcall_engine::herdr::{Compat, guard};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run(
    socket: Option<PathBuf>,
    exit_after: Option<u64>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let env = Env::from_process();
    let loaded = config::load(&env)?;
    if socket.is_none() && loaded.config.herdr.mode == HerdrMode::Off {
        println!(
            "herdr.mode = \"off\" in {}; nothing to do (pass --socket to override)",
            loaded.source.label()
        );
        return Ok(ExitCode::SUCCESS);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(main_loop(
        env,
        loaded.config.herdr.session.clone(),
        socket,
        exit_after,
    ))
}

async fn main_loop(
    env: Env,
    pinned_session: Option<String>,
    socket: Option<PathBuf>,
    exit_after: Option<u64>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    println!("lastcall hello-herdr (events are hints; snapshots are truth)");

    // 1. Session discovery (or the --socket override).
    let (path, source) = match socket {
        Some(path) => (path, "--socket".to_string()),
        None => {
            match discovery::discover(&env, pinned_session.as_deref(), |p: PathBuf| async move {
                socket_answers_ping(&p, Duration::from_secs(1)).await
            })
            .await
            {
                Discovery::Socket { path, source } => (path, format!("{source:?}")),
                other => {
                    println!("standalone: {}", other.notice().unwrap_or_default());
                    return Ok(ExitCode::from(1));
                }
            }
        }
    };
    println!("session: {} ({source})", path.display());

    // 2. ping and the compat verdict.
    let transport = SocketTransport::new(&path, REQUEST_TIMEOUT);
    let compat = guard::probe(&transport).await;
    match &compat {
        Compat::Ok { version, protocol } => {
            println!(
                "ping: herdr {version} protocol {protocol} (supported: {})",
                guard::SUPPORTED_PROTOCOLS
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join("/")
            );
            println!("compat: ok");
        }
        Compat::Mismatch {
            version,
            server_protocol,
            notice,
        } => {
            println!("ping: herdr {version} protocol {server_protocol}");
            println!("compat: mismatch — {notice}");
            return Ok(ExitCode::from(1));
        }
        Compat::Absent { reason } => {
            println!("ping: failed — {reason}");
            return Ok(ExitCode::from(1));
        }
    }

    // 3. Bootstrap and stream.
    let exit_on_disconnect = source == "--socket";
    let (handle, mut rx) = Client::spawn(
        transport,
        ClientTimings::default(),
        ClientOptions {
            reconnect: !exit_on_disconnect,
        },
    );
    let deadline = exit_after.map(|secs| tokio::time::sleep(Duration::from_secs(secs)));
    tokio::pin!(deadline);
    let mut printed_table = false;
    let code = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("ctrl-c: exiting");
                break ExitCode::SUCCESS;
            }
            _ = async {
                match deadline.as_mut().as_pin_mut() {
                    Some(sleep) => sleep.await,
                    None => std::future::pending().await,
                }
            } => {
                println!("exit-after {} s reached: exiting", exit_after.unwrap_or(0));
                break ExitCode::SUCCESS;
            }
            event = rx.recv() => {
                let Some(event) = event else {
                    println!("client ended");
                    break ExitCode::SUCCESS;
                };
                match event {
                    HerdrEvent::Connected { version, protocol } => {
                        println!("connected: herdr {version} protocol {protocol}");
                        if !printed_table
                            && let Some(cache) = handle.snapshot()
                        {
                            print_table(&cache);
                            printed_table = true;
                        }
                    }
                    HerdrEvent::Disconnected { reason } => {
                        println!("disconnected: {reason}");
                        if exit_on_disconnect {
                            println!("--socket given: exiting instead of reconnecting");
                            break ExitCode::SUCCESS;
                        }
                        printed_table = false;
                    }
                    HerdrEvent::Standalone { notice } => {
                        println!("standalone: {notice}");
                        if exit_on_disconnect {
                            break ExitCode::from(1);
                        }
                    }
                    HerdrEvent::Lifecycle(event) => println!("event: {}", event.summary()),
                    HerdrEvent::Resync(ResyncTarget::Snapshot) => println!("resync: snapshot"),
                    HerdrEvent::Resync(ResyncTarget::PaneGet(id)) => println!("resync: pane.get {id}"),
                    HerdrEvent::AgentStatusChanged { pane_id, from, to, agent, .. } => {
                        println!(
                            "[{pane_id}] {} {} → {to}",
                            agent.as_deref().unwrap_or("(no agent)"),
                            from.as_ref().map(ToString::to_string).unwrap_or_else(|| "(none)".into())
                        );
                    }
                    HerdrEvent::PaneAssociation { .. } => {}
                    HerdrEvent::WorktreeChanged { change, workspace_id, path, branch } => {
                        println!(
                            "worktree: {change:?} ws={workspace_id} path={path} branch={}",
                            branch.as_deref().unwrap_or("-")
                        );
                    }
                    HerdrEvent::StatusStreamOpened { pane_id } => println!("status: subscribed {pane_id}"),
                    HerdrEvent::StatusStreamClosed { pane_id, reason } => {
                        println!("status: closed {pane_id} ({reason})");
                    }
                }
            }
        }
    };
    handle.shutdown().await;
    Ok(code)
}

fn print_table(cache: &Cache) {
    println!("workspaces ({}):", cache.workspaces.len());
    for ws in cache.workspaces.values() {
        let repo = ws
            .worktree
            .as_ref()
            .map(|w| format!("  repo={} ({})", w.repo_name, w.checkout_path))
            .unwrap_or_default();
        println!(
            "  {}  label={:?}  status={}  active_tab={}{repo}",
            ws.workspace_id, ws.label, ws.agent_status, ws.active_tab_id
        );
    }
    println!("panes ({}):", cache.panes.len());
    for pane in cache.panes.values() {
        let info = &pane.info;
        println!(
            "  {}  tab={}  agent={}  status={}  cwd={}",
            info.pane_id,
            info.tab_id,
            info.agent_label().unwrap_or("-"),
            pane.status
                .as_ref()
                .map_or("?".to_string(), ToString::to_string),
            info.foreground_cwd
                .as_deref()
                .or(info.cwd.as_deref())
                .unwrap_or("-")
        );
    }
    println!("agents ({}):", cache.agents.len());
    for agent in cache.agents.values() {
        println!(
            "  {}  {}  {}",
            agent.pane_id,
            agent
                .display_agent
                .as_deref()
                .or(agent.agent.as_deref())
                .unwrap_or("-"),
            agent.agent_status
        );
    }
    println!("streaming (ctrl-c to stop)...");
}
