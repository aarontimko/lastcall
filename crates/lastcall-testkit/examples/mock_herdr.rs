//! `mock_herdr --socket <path> --snapshot <json> --events <jsonl> [--snapshot-after <json>]
//! [--protocol N] [--gap-ms N]`
//!
//! Serves a scripted herdr session over a real Unix socket: `ping`, `session.snapshot`,
//! `pane.get`, `pane.list`, the lifecycle subscription, and per-pane status subscriptions for
//! every pane in the snapshot. The event script is routed by shape: dotted
//! `pane.agent_status_changed` lines play on their pane's status stream, everything else on
//! the lifecycle stream, each line `--gap-ms` after the previous one on that stream (from the
//! subscription's ack). With `--snapshot-after`, the served snapshot is swapped for that file
//! once every status script has played — so a later `tab_focused` in the script makes the
//! client's focus resync observe the flipped state (the silent done → idle of §5.7). Exits
//! when every script has played and no connection remains.
//!
//! This is what `just probe-hello` runs; an example does not violate the testkit's
//! dev-dependency-only rule.

use std::path::PathBuf;
use std::time::Duration;

use lastcall_testkit::mock_herdr::{MockHerdr, ScriptedEvent};

struct Args {
    socket: PathBuf,
    snapshot: PathBuf,
    events: PathBuf,
    snapshot_after: Option<PathBuf>,
    protocol: u32,
    gap: Duration,
}

fn parse_args() -> Result<Args, String> {
    let mut socket = None;
    let mut snapshot = None;
    let mut events = None;
    let mut snapshot_after = None;
    let mut protocol = 21;
    let mut gap = Duration::from_millis(250);
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("missing value for {name}"));
        match arg.as_str() {
            "--socket" => socket = Some(PathBuf::from(value("--socket")?)),
            "--snapshot" => snapshot = Some(PathBuf::from(value("--snapshot")?)),
            "--events" => events = Some(PathBuf::from(value("--events")?)),
            "--snapshot-after" => {
                snapshot_after = Some(PathBuf::from(value("--snapshot-after")?));
            }
            "--protocol" => {
                protocol = value("--protocol")?
                    .parse()
                    .map_err(|e| format!("--protocol: {e}"))?;
            }
            "--gap-ms" => {
                gap = Duration::from_millis(
                    value("--gap-ms")?
                        .parse()
                        .map_err(|e| format!("--gap-ms: {e}"))?,
                );
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        socket: socket.ok_or("--socket is required")?,
        snapshot: snapshot.ok_or("--snapshot is required")?,
        events: events.ok_or("--events is required")?,
        snapshot_after,
        protocol,
        gap,
    })
}

fn read_json(path: &PathBuf) -> serde_json::Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("mock_herdr: cannot read {}: {e}", path.display());
        std::process::exit(2);
    });
    serde_json::from_str(&text).unwrap_or_else(|e| {
        eprintln!("mock_herdr: {} is not JSON: {e}", path.display());
        std::process::exit(2);
    })
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("mock_herdr: {err}");
            eprintln!(
                "usage: mock_herdr --socket <path> --snapshot <json> --events <jsonl> [--protocol N] [--gap-ms N]"
            );
            std::process::exit(2);
        }
    };
    let snapshot = read_json(&args.snapshot);
    let snapshot_after = args.snapshot_after.as_ref().map(read_json);
    let events_text = std::fs::read_to_string(&args.events).unwrap_or_else(|e| {
        eprintln!("mock_herdr: cannot read {}: {e}", args.events.display());
        std::process::exit(2);
    });
    let (lifecycle, status) =
        ScriptedEvent::route(ScriptedEvent::from_jsonl(&events_text, args.gap));

    let mut builder = MockHerdr::builder()
        .protocol(args.protocol)
        .snapshot(snapshot)
        .lifecycle_events(lifecycle);
    for (pane_id, script) in status {
        builder = builder.status_events(&pane_id, script);
    }
    let mock = builder.serve(&args.socket).await.unwrap_or_else(|e| {
        eprintln!("mock_herdr: cannot bind {}: {e}", args.socket.display());
        std::process::exit(2);
    });
    println!(
        "mock_herdr: serving {} (protocol {})",
        args.socket.display(),
        args.protocol
    );

    // Swap in the "after" snapshot once every status script has played; exit when every
    // script has played and no connection remains.
    let mut served_any = false;
    let mut swapped = snapshot_after.is_none();
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if mock.connections_open() > 0 {
            served_any = true;
        }
        if !swapped
            && mock.all_status_scripts_played()
            && let Some(after) = snapshot_after.clone()
        {
            mock.set_snapshot(after);
            println!("mock_herdr: status scripts played; serving --snapshot-after from now on");
            swapped = true;
        }
        if served_any
            && mock.connections_open() == 0
            && mock.lifecycle_script_played()
            && mock.all_status_scripts_played()
        {
            break;
        }
    }
    let requests = mock.requests();
    println!(
        "mock_herdr: done; {} requests: {}",
        requests.len(),
        requests
            .iter()
            .map(|r| r.method.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    mock.shutdown().await;
}
