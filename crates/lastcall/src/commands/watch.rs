//! `lastcall watch [--json] [--exit-after <secs>] [--poll <secs>]`: run the engine's
//! watcher loop and print one line per [`EngineEvent`] until Ctrl-C or the deadline.
//! `--poll` shortens both polling backstops (HEAD every 10 s, rescan every 30 s by
//! default) for hosts whose filesystem events are late or missing.

use std::process::ExitCode;
use std::time::Duration;

use lastcall_engine::config;
use lastcall_engine::engine::Engine;
use lastcall_engine::env::Env;
use lastcall_engine::status::{RowStatus, row_line};
use lastcall_engine::watcher::EngineEvent;

pub fn run(
    json: bool,
    exit_after: Option<u64>,
    poll: Option<u64>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let env = Env::from_process();
    let loaded = config::load(&env)?;
    let resolved = loaded.resolve(env.cwd());
    let engine = match Engine::open(&loaded, &resolved, &env, crate::commands::engine_options()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("lastcall: {e}");
            return Ok(ExitCode::from(1));
        }
    };
    for n in engine.notices() {
        print_line(
            json,
            &serde_json::json!({"event": "notice", "root": null, "text": n}),
            || format!("notice: {n}"),
        );
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let timings = super::poll_timings(poll);
    let outcome = runtime.block_on(async move {
        let mut watcher = engine.run(timings);
        let deadline = exit_after.map(|secs| tokio::time::sleep(Duration::from_secs(secs)));
        tokio::pin!(deadline);
        let code = loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    print_line(json, &serde_json::json!({"event": "exit", "reason": "ctrl-c"}), || "ctrl-c: exiting".to_owned());
                    break ExitCode::SUCCESS;
                }
                _ = async {
                    match deadline.as_mut().as_pin_mut() {
                        Some(sleep) => sleep.await,
                        None => std::future::pending().await,
                    }
                } => {
                    print_line(json, &serde_json::json!({"event": "exit", "reason": "exit-after"}), || {
                        format!("exit-after {} s reached: exiting", exit_after.unwrap_or(0))
                    });
                    break ExitCode::SUCCESS;
                }
                event = watcher.events.recv() => {
                    let Some(event) = event else {
                        print_line(json, &serde_json::json!({"event": "exit", "reason": "watcher ended"}), || "watcher ended".to_owned());
                        break ExitCode::SUCCESS;
                    };
                    print_event(json, &event);
                }
            }
        };
        watcher.join().await;
        Ok(code)
    });
    // A watch installation still registering its platform stream must not hold the exit.
    runtime.shutdown_timeout(Duration::from_millis(500));
    outcome
}

fn print_line(json: bool, value: &serde_json::Value, human: impl FnOnce() -> String) {
    if json {
        println!("{value}");
    } else {
        println!("{}", human());
    }
}

fn name(root: &std::path::Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string())
}

fn print_event(json: bool, event: &EngineEvent) {
    match event {
        EngineEvent::Pile { root, seq, pile } => {
            let rows: Vec<RowStatus> = pile.rows.iter().map(RowStatus::of).collect();
            print_line(
                json,
                &serde_json::json!({
                    "event": "pile",
                    "root": root,
                    "seq": seq,
                    "pending": rows,
                    "omitted": pile.omitted,
                    "notices": pile.notices,
                }),
                || {
                    let mut s = format!("{} #{seq}  {} pending", name(root), rows.len());
                    for r in &rows {
                        s.push_str(&format!("\n  {}", row_line(r)));
                    }
                    for n in &pile.notices {
                        s.push_str(&format!("\n  notice: {n}"));
                    }
                    s
                },
            );
        }
        EngineEvent::Head {
            root,
            from,
            to,
            branch,
            notice,
        } => print_line(
            json,
            &serde_json::json!({
                "event": "head",
                "root": root,
                "from": from,
                "to": to,
                "branch": branch,
                "notice": notice,
            }),
            || match notice {
                Some(n) => format!("{}  {n}", name(root)),
                None => format!(
                    "{}  HEAD {} → {}",
                    name(root),
                    from.as_ref()
                        .map(|o| o.as_str()[..7].to_owned())
                        .unwrap_or_else(|| "none".into()),
                    to.as_ref()
                        .map(|o| o.as_str()[..7].to_owned())
                        .unwrap_or_else(|| "none".into())
                ),
            },
        ),
        EngineEvent::Scanned { root, rows } => print_line(
            json,
            &serde_json::json!({
                "event": "scanned",
                "root": root,
                "rows": rows,
            }),
            || format!("{}  scanned · {rows} pending", name(root)),
        ),
        EngineEvent::RootsChanged(changed) => print_line(
            json,
            &serde_json::json!({
                "event": "roots_changed",
                "added": changed.added,
                "removed": changed.removed,
            }),
            || {
                format!(
                    "roots changed: +{} −{}",
                    changed.added.len(),
                    changed.removed.len()
                )
            },
        ),
        EngineEvent::Notice { root, text } => print_line(
            json,
            &serde_json::json!({"event": "notice", "root": root, "text": text}),
            || match root {
                Some(r) => format!("{}  notice: {text}", name(r)),
                None => format!("notice: {text}"),
            },
        ),
    }
}
