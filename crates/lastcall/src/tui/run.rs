//! The event loop (kickoff deliverable 6).
//!
//! `run` builds the tokio runtime exactly as `commands/watch.rs` does, starts the engine's
//! watcher, seeds the app with the engine's roots (no scan has run yet, so the first frame
//! is the empty state with `scanning N roots…` until piles arrive), and then loops over one
//! `select!`: watcher events → `App::apply`; terminal events (a detached reader thread) →
//! `to_action` → `App::handle`; the loop's own finished engine work (`Local`) → the app;
//! a 1 s tick; Ctrl-C (dead under raw mode, kept for `kill -INT`) and SIGTERM.
//!
//! The screen is redrawn at most once per iteration and only when a reducer returned
//! `Changed::Yes`. The `HitMap` of the last frame lives here, not in `App`, and is dropped
//! on `Resize` so a press between a resize and the next render hits nothing.
//!
//! Every engine call goes through `watcher::blocking` on a spawned task; the UI task never
//! holds the engine mutex (the gate grep for a mutex lock finds nothing under `tui/`). Quit is
//! `restore()` first — the shell is sane even if shutdown hangs — then a bounded
//! `watcher.join()`, then a bounded runtime shutdown ([`shut_down`]).
//!
//! The reader thread is never joined: `crossterm::event::read()` blocks in `mio::Poll` on
//! the tty and nothing in the restore sequence wakes it, so it polls with a 50 ms timeout
//! under a stop flag and exits on its own shortly after the loop ends.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::Event;
use lastcall_engine::engine::{AcceptRequest, Engine};
use lastcall_engine::env::Env;
use lastcall_engine::herdr::HerdrEvent;
use lastcall_engine::herdr::client::ClientHandle;
use lastcall_engine::herdr::transport::SocketTransport;
use lastcall_engine::scan::Pile;
use lastcall_engine::watcher::{EngineEvent, EngineTimings, Watcher, blocking};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use super::app::{AcceptFailed, AcceptResult, App, Changed, Effect, RootMeta};
use super::herdr::{self, HerdrLink, HerdrPlan, HerdrUpdate, Link, ToastMsg};
use super::input::{Action, Key, Keymap, modal_action, pointer, to_action};
use super::render::{HitMap, Pane, render};
use super::term;

/// How long the quit path waits for the watcher, and then for the runtime.
pub const SHUTDOWN_BUDGET: Duration = Duration::from_millis(500);
/// The reader thread's poll timeout: how long after the loop ends it can linger.
pub const INPUT_POLL: Duration = Duration::from_millis(50);
/// Status-line ages advance this often.
pub const TICK: Duration = Duration::from_secs(1);
/// How long a burst of `worktree.*` events is coalesced before one discovery rescan
/// (deliverable 7): long enough that `git worktree add`'s own file churn settles, short
/// enough that the new repo appears while the user is still looking at the screen.
pub const WORKTREE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Results of the engine work the loop spawned on the app's behalf.
#[derive(Debug, Clone, PartialEq)]
pub enum Local {
    /// One root's pile from a `Refresh` (`Engine::scan_all`), with the seq of that scan so
    /// the reducer can drop an older watcher pile that lands after it.
    Pile(PathBuf, u64, Pile),
    /// An `Effect::Accept` finished: one result per root it covered.
    Accepted(Vec<(PathBuf, AcceptResult)>),
    /// The engine's roots after a `SyncRoots`.
    Roots(Vec<RootMeta>),
    /// The `Refresh` finished (all its piles were sent first).
    RefreshDone,
    /// A root's scan failed during a `Refresh`; its previous pile stays.
    Notice(Option<PathBuf>, String),
    /// An engine task died (panicked): the panic hook has already restored the terminal,
    /// so the loop must end. `run` reports the text on stderr and fails.
    Fatal(String),
    /// A herdr request the loop made off the UI task came back (`agent.focus`, a toast).
    Herdr(HerdrUpdate),
}

/// The terminal-free core of the loop: the app, the keymap, and the hit map of the last
/// frame (`None` before the first render and again after a `Resize`).
#[derive(Debug)]
pub struct Ui {
    pub app: App,
    pub keymap: Keymap,
    pub hits: Option<HitMap>,
}

impl Ui {
    /// Wrap `app`, storing the keymap's table into it so the hint line and the help overlay
    /// show the effective bindings.
    pub fn new(mut app: App, keymap: Keymap) -> Self {
        app.keymap = keymap.table();
        Self {
            app,
            keymap,
            hits: None,
        }
    }

    /// Fold one terminal event in. Keys resolve through the keymap; a press resolves
    /// through the last hit map (nothing when there is none); the wheel moves the nav
    /// selection when the pointer is over the nav and scrolls the diff otherwise; a resize
    /// invalidates the hit map before the app sees it. While the confirm modal is open only
    /// its own keys, the `quit` keys and a resize get through; the mouse is dropped here
    /// (the wheel over the nav would otherwise reach `move_selection` around the gate).
    pub fn event(&mut self, event: &Event) -> (Changed, Option<Effect>) {
        // The confirm modal answers to its own keys (`y`/`Enter`, `n`/`Esc`), consulted
        // before the keymap, and to the keymap's `quit` keys (`q` and ctrl-c quit by
        // default, everywhere — the help overlay lets `Quit` through the same way); every
        // other key is swallowed. `Esc` is the modal's cancel first, so it never quits.
        // Every non-key event but `Resize` is dropped before it can touch the app.
        if self.app.confirm.is_some() {
            match event {
                Event::Key(k) => {
                    return match Key::of(k).and_then(modal_action) {
                        Some(action) => self.app.handle(action),
                        None => match to_action(event, &self.keymap) {
                            Some(Action::Quit) => self.app.handle(Action::Quit),
                            _ => (Changed::No, None),
                        },
                    };
                }
                Event::Resize(..) => {}
                _ => return (Changed::No, None),
            }
        }
        let Some(action) = to_action(event, &self.keymap) else {
            return (Changed::No, None);
        };
        match action {
            Action::Resize(..) => {
                self.hits = None;
                self.app.handle(action)
            }
            Action::Press(x, y) => match self.hits.as_ref().and_then(|h| h.at(x, y)).cloned() {
                Some(target) => self.app.hit(target),
                None => (Changed::No, None),
            },
            Action::ScrollUp(_) | Action::ScrollDown(_) if !self.app.help => {
                let pane = pointer(event).and_then(|(x, y)| self.hits.as_ref()?.pane_at(x, y));
                match pane {
                    Some(Pane::Nav) => {
                        let delta = if matches!(action, Action::ScrollUp(_)) {
                            -1
                        } else {
                            1
                        };
                        (self.app.move_selection(delta), None)
                    }
                    _ => self.app.handle(action),
                }
            }
            other => self.app.handle(other),
        }
    }

    /// Fold one watcher event in.
    pub fn engine(&mut self, event: EngineEvent) -> (Changed, Option<Effect>) {
        self.app.apply(event)
    }

    /// Fold one finished piece of the loop's own engine work in.
    pub fn local(&mut self, local: Local) -> (Changed, Option<Effect>) {
        match local {
            Local::Pile(root, seq, pile) => self.app.apply(EngineEvent::Pile { root, seq, pile }),
            Local::Accepted(results) => (self.app.accepted(results), None),
            Local::Roots(metas) => (self.app.sync_roots(metas), None),
            Local::RefreshDone => (self.app.refresh_done(), None),
            Local::Notice(root, text) => self.app.apply(EngineEvent::Notice { root, text }),
            Local::Fatal(text) => {
                self.app.set_status(text);
                (Changed::No, Some(Effect::Quit))
            }
            Local::Herdr(update) => self.app.handle(Action::Herdr(update)),
        }
    }

    /// Record the hit map a render produced.
    pub fn rendered(&mut self, hits: HitMap) {
        self.hits = Some(hits);
    }
}

/// The quit sequence's three steps, in the only safe order (see [`shut_down`]).
pub trait Shutdown {
    /// Leave the alternate screen, drop mouse capture and raw mode.
    fn restore(&mut self);
    /// Stop the herdr client and wait for it, bounded (deliverable 4: a dead socket must
    /// not hold the quit path open).
    fn shutdown_herdr(&mut self);
    /// Stop the watcher and wait for it, bounded.
    fn join_watcher(&mut self);
    /// Shut the runtime down, bounded.
    fn shutdown_runtime(&mut self);
}

/// Restore the terminal **first** (the screen is sane even if shutdown hangs), then stop
/// the herdr client, then join the watcher, then shut the runtime down. Every step after
/// the first is bounded by [`SHUTDOWN_BUDGET`], so `q` always returns the shell.
pub fn shut_down(s: &mut impl Shutdown) {
    s.restore();
    s.shutdown_herdr();
    s.join_watcher();
    s.shutdown_runtime();
}

type Screen = Terminal<CrosstermBackend<io::Stdout>>;

struct RealShutdown {
    terminal: Option<Screen>,
    guard: Option<term::TerminalGuard>,
    runtime: Option<tokio::runtime::Runtime>,
    watcher: Option<Watcher>,
    herdr: Option<ClientHandle>,
}

impl Shutdown for RealShutdown {
    fn restore(&mut self) {
        if let Some(mut t) = self.terminal.take() {
            let _ = t.show_cursor();
        }
        drop(self.guard.take());
        term::restore();
    }

    fn shutdown_herdr(&mut self) {
        if let (Some(h), Some(rt)) = (self.herdr.take(), self.runtime.as_ref()) {
            // `ClientHandle::drop` aborts the task anyway; this only gives the connections
            // a bounded chance to close politely first.
            block_bounded(rt, SHUTDOWN_BUDGET, h.shutdown());
        }
    }

    fn join_watcher(&mut self) {
        if let (Some(w), Some(rt)) = (self.watcher.take(), self.runtime.as_ref()) {
            block_bounded(rt, SHUTDOWN_BUDGET, w.join());
        }
    }

    fn shutdown_runtime(&mut self) {
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(SHUTDOWN_BUDGET);
        }
    }
}

/// Block on `fut` for at most `budget`; `None` when it did not finish. The timeout is
/// created *inside* `block_on`: `tokio::time::timeout` arms its timer at construction and
/// panics ("no reactor running") when built outside a runtime context, which the first
/// pty run of the quit path hit.
fn block_bounded<F: Future>(
    rt: &tokio::runtime::Runtime,
    budget: Duration,
    fut: F,
) -> Option<F::Output> {
    rt.block_on(async { tokio::time::timeout(budget, fut).await.ok() })
}

/// The detached reader thread: `poll(INPUT_POLL)` + `read()` under a stop flag, forwarding
/// every event. Returns the flag; the thread is never joined (module docs).
fn spawn_input(tx: mpsc::UnboundedSender<Event>) -> io::Result<Arc<AtomicBool>> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("lastcall-input".to_owned())
        .spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match crossterm::event::poll(INPUT_POLL) {
                    Ok(true) => match crossterm::event::read() {
                        Ok(event) => {
                            if tx.send(event).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    },
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        })?;
    Ok(stop)
}

fn root_metas(engine: &mut Engine) -> Vec<RootMeta> {
    engine.roots().iter().map(|r| RootMeta::of(r)).collect()
}

/// `Effect::Refresh`: scan every root off the UI task, one `Local::Pile` per root, then
/// `Local::RefreshDone`.
fn spawn_refresh(engine: &Arc<Mutex<Engine>>, tx: mpsc::UnboundedSender<Local>) {
    let engine = engine.clone();
    tokio::spawn(async move {
        let scan = tokio::spawn(async move { blocking(&engine, |e| e.scan_all()).await });
        let Some(results) = joined(scan, &tx, "refresh").await else {
            return;
        };
        for (root, seq, result) in results {
            let local = match result {
                Ok(pile) => Local::Pile(root, seq, pile),
                Err(e) => Local::Notice(Some(root), format!("scan failed: {e}")),
            };
            if tx.send(local).is_err() {
                return;
            }
        }
        let _ = tx.send(Local::RefreshDone);
    });
}

/// `Effect::Accept`: every root's `Engine::accept` (the op and its rescan) in one
/// `blocking` closure, so one critical section covers the whole request and no watcher scan
/// interleaves; the results come back as one `Local::Accepted`.
fn spawn_accept(
    engine: &Arc<Mutex<Engine>>,
    tx: mpsc::UnboundedSender<Local>,
    reqs: Vec<(PathBuf, AcceptRequest)>,
) {
    let engine = engine.clone();
    tokio::spawn(async move {
        let accept = tokio::spawn(async move {
            blocking(&engine, move |e| {
                reqs.into_iter()
                    .map(|(root, req)| {
                        let result = e.accept(&root, req).map_err(|e| AcceptFailed::of(&e));
                        (root, result)
                    })
                    .collect::<Vec<_>>()
            })
            .await
        });
        if let Some(results) = joined(accept, &tx, "accept").await {
            let _ = tx.send(Local::Accepted(results));
        }
    });
}

/// `Effect::SyncRoots`: re-read every root's metadata off the UI task.
fn spawn_sync_roots(engine: &Arc<Mutex<Engine>>, tx: mpsc::UnboundedSender<Local>) {
    let engine = engine.clone();
    tokio::spawn(async move {
        let read = tokio::spawn(async move { blocking(&engine, root_metas).await });
        if let Some(metas) = joined(read, &tx, "root sync").await {
            let _ = tx.send(Local::Roots(metas));
        }
    });
}

/// Await an engine task; if it died (`blocking` re-panics a panicking closure, and the panic
/// hook has already restored the terminal) tell the loop to stop instead of leaving it
/// drawing onto a cooked terminal with `refreshing` stuck.
async fn joined<T>(
    task: tokio::task::JoinHandle<T>,
    tx: &mpsc::UnboundedSender<Local>,
    what: &str,
) -> Option<T> {
    match task.await {
        Ok(value) => Some(value),
        Err(e) => {
            let _ = tx.send(Local::Fatal(format!("{what} failed: {e}")));
            None
        }
    }
}

/// The loop's half of the herdr link: what it needs to derive, to request, and to stop.
/// The reducer sees none of this (§6.6).
#[derive(Default)]
struct Herdr {
    handle: Option<ClientHandle>,
    transport: Option<SocketTransport>,
    events: Option<mpsc::Receiver<HerdrEvent>>,
    toast: Option<mpsc::UnboundedSender<ToastMsg>>,
    workspace_id: Option<String>,
}

impl Herdr {
    /// Take the link apart into the loop's fields and start the toast task.
    fn adopt(link: HerdrLink, updates: mpsc::UnboundedSender<Local>) -> Herdr {
        let toast = link.plan.toast.then(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            let transport = link.transport.clone();
            tokio::spawn(async move {
                let (utx, mut urx) = mpsc::unbounded_channel();
                let send = tokio::spawn(async move {
                    while let Some(update) = urx.recv().await {
                        if updates.send(Local::Herdr(update)).is_err() {
                            break;
                        }
                    }
                });
                herdr::toast_loop(transport, rx, utx).await;
                send.abort();
            });
            tx
        });
        Herdr {
            handle: Some(link.handle),
            transport: Some(link.transport),
            events: Some(link.events),
            toast,
            workspace_id: link.plan.workspace_id,
        }
    }
}

/// The fourth `select!` arm's future: the client's event stream, or forever when there is
/// no link (`mode = "off"`, or a standalone start).
async fn herdr_recv(events: &mut Option<mpsc::Receiver<HerdrEvent>>) -> Option<HerdrEvent> {
    match events {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Re-derive the association and the scope from a fresh snapshot and fold both in
/// (deliverable 4's trigger set, plus `Local::Roots`).
fn herdr_rederive(ui: &mut Ui, link: &Herdr) -> (Changed, Option<Effect>) {
    let Some(cache) = link.handle.as_ref().and_then(ClientHandle::snapshot) else {
        return (Changed::No, None);
    };
    let metas: Vec<RootMeta> = ui.app.roots.values().map(|v| v.meta.clone()).collect();
    let scope = link
        .workspace_id
        .as_deref()
        .and_then(|id| herdr::derive_scope(&cache, &metas, id));
    let roots = herdr::derive(&cache, &metas);
    let (c1, _) = ui.app.handle(Action::Herdr(HerdrUpdate::Scope(scope)));
    let (c2, effect) = ui.app.handle(Action::Herdr(HerdrUpdate::Roots(roots)));
    (c1.or(c2), effect)
}

/// `Effect::Focus`: `agent.focus` off the UI task; the verdict comes back as
/// `Local::Herdr(Focused)`.
fn spawn_focus(
    transport: Option<SocketTransport>,
    tx: mpsc::UnboundedSender<Local>,
    pane: String,
    label: String,
) {
    let Some(transport) = transport else {
        return;
    };
    tokio::spawn(async move {
        let update = match herdr::focus(&transport, &pane).await {
            Ok(()) => HerdrUpdate::Focused(Ok(label)),
            Err(e) => HerdrUpdate::Focused(Err(e)),
        };
        let _ = tx.send(Local::Herdr(update));
    });
}

fn draw(terminal: &mut Screen, ui: &mut Ui) -> io::Result<()> {
    let mut hits = HitMap::default();
    terminal.draw(|frame| hits = render(&ui.app, frame))?;
    ui.rendered(hits);
    Ok(())
}

/// SIGINT and SIGTERM, registered *before* the terminal is taken so a signal in the gap
/// between `term::enter()` and the first `select!` still goes through the restore path
/// instead of the default disposition (which would leave raw mode and the alternate
/// screen on).
struct Signals {
    #[cfg(unix)]
    int: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    term: Option<tokio::signal::unix::Signal>,
}

impl Signals {
    /// Needs a runtime context (`Runtime::enter`).
    fn register() -> Signals {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Signals {
                int: signal(SignalKind::interrupt()).ok(),
                term: signal(SignalKind::terminate()).ok(),
            }
        }
        #[cfg(not(unix))]
        {
            Signals {}
        }
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        match (&mut self.int, &mut self.term) {
            (Some(int), Some(term)) => tokio::select! {
                _ = int.recv() => {}
                _ = term.recv() => {}
            },
            (Some(int), None) => {
                int.recv().await;
            }
            (None, Some(term)) => {
                term.recv().await;
            }
            (None, None) => std::future::pending::<()>().await,
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// Take the terminal and run the TUI until `q`/Ctrl-C/SIGTERM (exit 0) or the watcher
/// ends (status notice, exit 0). The caller has already checked that stdout is a terminal
/// and that the keymap parsed; this enters raw mode and always restores it.
pub fn run(
    engine: Engine,
    timings: EngineTimings,
    keymap: Keymap,
    env: Env,
    plan: HerdrPlan,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut signals = {
        let _ctx = runtime.enter();
        Signals::register()
    };
    let guard = term::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut ui = Ui::new(App::new(), keymap);
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Event>();
    let stop = spawn_input(input_tx)?;
    let (local_tx, mut local_rx) = mpsc::unbounded_channel::<Local>();

    let mut link = Herdr::default();
    let (outcome, watcher) = runtime.block_on(async {
        let mut watcher = engine.run(timings);
        let outcome: io::Result<ExitCode> = async {
            let metas = blocking(&watcher.engine, root_metas).await;
            let n = metas.len();
            ui.app.sync_roots(metas);
            ui.app.set_status(format!(
                "scanning {n} root{}…",
                if n == 1 { "" } else { "s" }
            ));
            if let Ok((w, h)) = crossterm::terminal::size() {
                ui.app.handle(Action::Resize(w, h));
            }
            draw(&mut terminal, &mut ui)?;

            // The link is opened after the first frame: discovery and the protocol guard
            // are bounded, but the empty state is on screen before either runs.
            ui.app.herdr.scoped = plan.scoped;
            ui.app.herdr.toast = plan.toast;
            match herdr::connect(&env, plan).await {
                Ok(open) => link = Herdr::adopt(open, local_tx.clone()),
                Err(badge) => ui.app.herdr.link = badge,
            }
            if ui.app.herdr.link != Link::Off {
                draw(&mut terminal, &mut ui)?;
            }

            let mut tick = tokio::time::interval(TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // the immediate first tick
            let mut fatal: Option<String> = None;
            // Set by a `worktree.*` event, fired once the burst has been quiet this long.
            let mut worktree_due: Option<tokio::time::Instant> = None;
            loop {
                // Copied out so the timer future borrows nothing a handler assigns to.
                let due = worktree_due;
                let mut rescan = false;
                let (changed, effect) = tokio::select! {
                    _ = signals.recv() => break,
                    event = watcher.events.recv() => match event {
                        Some(event) => ui.engine(event),
                        None => {
                            tracing::warn!("watcher ended; exiting");
                            ui.app.set_status("watcher ended");
                            draw(&mut terminal, &mut ui)?;
                            break;
                        }
                    },
                    event = input_rx.recv() => match event {
                        Some(event) => ui.event(&event),
                        None => break, // the reader thread died
                    },
                    local = local_rx.recv() => match local {
                        Some(Local::Fatal(text)) => {
                            fatal = Some(text);
                            break;
                        }
                        Some(Local::Roots(metas)) => {
                            // The root set moved under a live cache: re-associate.
                            let (c, _) = ui.local(Local::Roots(metas));
                            let (c2, e) = herdr_rederive(&mut ui, &link);
                            (c.or(c2), e)
                        }
                        Some(local) => ui.local(local),
                        None => (Changed::No, None),
                    },
                    event = herdr_recv(&mut link.events) => match event {
                        Some(event) => {
                            if herdr::triggers_rescan(&event) {
                                worktree_due =
                                    Some(tokio::time::Instant::now() + WORKTREE_DEBOUNCE);
                            }
                            let mut changed = Changed::No;
                            let mut effect = None;
                            if let Some(update) = herdr::update_of(&event) {
                                let (c, e) = ui.app.handle(Action::Herdr(update));
                                changed = changed.or(c);
                                effect = effect.or(e);
                            }
                            if herdr::rederives(&event) {
                                let (c, e) = herdr_rederive(&mut ui, &link);
                                changed = changed.or(c);
                                effect = effect.or(e);
                            }
                            (changed, effect)
                        }
                        // The client task ended (it never does with `reconnect: true`).
                        None => {
                            link.events = None;
                            (Changed::No, None)
                        }
                    },
                    _ = async {
                        match due {
                            Some(at) => tokio::time::sleep_until(at).await,
                            None => std::future::pending().await,
                        }
                    } => {
                        rescan = true;
                        (Changed::No, None)
                    }
                    _ = tick.tick() => ui.app.handle(Action::Tick),
                };
                if rescan {
                    // Deliverable 7: the event is only a trigger — `roots::discover` decides
                    // what is a root, and `RootsChanged` + the new pile take the usual path.
                    worktree_due = None;
                    watcher.request_rescan();
                }
                match effect {
                    Some(Effect::Quit) => break,
                    Some(Effect::Refresh) => spawn_refresh(&watcher.engine, local_tx.clone()),
                    Some(Effect::SyncRoots) => spawn_sync_roots(&watcher.engine, local_tx.clone()),
                    Some(Effect::Accept(reqs)) => {
                        spawn_accept(&watcher.engine, local_tx.clone(), reqs)
                    }
                    Some(Effect::Focus(pane)) => {
                        let label = ui
                            .app
                            .herdr
                            .roots
                            .values()
                            .find(|f| f.pane.as_deref() == Some(pane.as_str()))
                            .map(|f| f.agent_label())
                            .unwrap_or_else(|| pane.clone());
                        spawn_focus(link.transport.clone(), local_tx.clone(), pane, label);
                    }
                    Some(Effect::Toast(request)) => {
                        if let Some(tx) = &link.toast {
                            for name in request.ready {
                                let _ = tx.send(ToastMsg::Ready(name));
                            }
                            for name in request.dropped {
                                let _ = tx.send(ToastMsg::Drop(name));
                            }
                        }
                    }
                    None => {}
                }
                if changed == Changed::Yes {
                    draw(&mut terminal, &mut ui)?;
                }
            }
            match fatal {
                Some(text) => Err(io::Error::other(text)),
                None => Ok(ExitCode::SUCCESS),
            }
        }
        .await;
        (outcome, watcher)
    });

    stop.store(true, Ordering::Relaxed);
    let mut real = RealShutdown {
        terminal: Some(terminal),
        guard: Some(guard),
        runtime: Some(runtime),
        watcher: Some(watcher),
        herdr: link.handle.take(),
    };
    shut_down(&mut real);
    Ok(outcome?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::testfix::*;
    use crate::tui::app::{Focus, Selection, Target};
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// Render `ui.app` at 100×30 into a `TestBackend` and record the hit map, as `draw`
    /// does on the real terminal.
    fn render_into(ui: &mut Ui) {
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut hits = HitMap::default();
        term.draw(|f| hits = render(&ui.app, f)).unwrap();
        ui.rendered(hits);
    }

    fn target_center(ui: &Ui, target: &Target) -> (u16, u16) {
        let (rect, _) = ui
            .hits
            .as_ref()
            .expect("rendered")
            .targets
            .iter()
            .find(|(_, t)| t == target)
            .unwrap_or_else(|| panic!("{target:?} on screen"));
        (rect.x + rect.width / 2, rect.y)
    }

    fn ui() -> Ui {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        Ui::new(app, Keymap::defaults())
    }

    fn frame_of(ui: &Ui) -> String {
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| {
            render(&ui.app, f);
        })
        .unwrap();
        term.backend().to_string()
    }

    /// The §11 hardening, at the loop's level: a watcher pile carrying a seq below the one
    /// the accept applied — a scan that was already running when the accept took the lock
    /// — reaches the app through either channel and leaves it, and the frame, untouched;
    /// the next newer pile is applied as usual.
    #[test]
    fn run_stale_watcher_pile_after_accept_is_dropped() {
        let mut ui = ui();
        assert_eq!(
            ui.local(Local::Pile(root("alpha"), 3, alpha_two_hunks())),
            (Changed::Yes, None)
        );
        ui.app.select(Some(row("alpha", "f1")));
        // Nav focus: `a` is accept-file.
        let (_, effect) = ui.event(&key(KeyCode::Char('a')));
        assert!(matches!(effect, Some(Effect::Accept(_))), "{effect:?}");
        assert!(ui.app.accepting.is_some());
        // The accept's own rescan comes back at seq 5 with `f1` gone.
        let after_accept = without(pile("alpha"), &["f1"]);
        assert_eq!(
            ui.local(Local::Accepted(vec![accepted_ok(
                "alpha",
                5,
                after_accept.clone()
            )])),
            (Changed::Yes, None)
        );
        assert!(ui.app.accepting.is_none());
        assert_eq!(ui.app.selection, Some(row("alpha", "f2")), "advanced");
        let before = ui.app.clone();
        let frame = frame_of(&ui);
        assert!(frame.contains("accepted f1"), "{frame}");

        // The pre-accept scan lands late, through the refresh channel …
        assert_eq!(
            ui.local(Local::Pile(root("alpha"), 4, alpha_two_hunks())),
            (Changed::No, None)
        );
        assert_eq!(ui.app, before);
        assert_eq!(frame_of(&ui), frame);
        // … and through the watcher's.
        assert_eq!(
            ui.engine(EngineEvent::Pile {
                root: root("alpha"),
                seq: 4,
                pile: alpha_two_hunks(),
            }),
            (Changed::No, None)
        );
        assert_eq!(ui.app, before);
        assert_eq!(frame_of(&ui), frame);
        assert_eq!(ui.app.seq[&root("alpha")], 5);

        // A newer scan is applied as ever.
        assert_eq!(
            ui.engine(EngineEvent::Pile {
                root: root("alpha"),
                seq: 6,
                pile: alpha_two_hunks(),
            }),
            (Changed::Yes, None)
        );
        assert_eq!(ui.app.roots[&root("alpha")].rows().len(), 2);
        assert_eq!(ui.app.seq[&root("alpha")], 6);
    }

    #[test]
    fn run_fatal_local_quits_and_a_dead_engine_task_becomes_fatal() {
        let mut ui = ui();
        assert_eq!(
            ui.local(Local::Fatal("refresh failed: boom".into())),
            (Changed::No, Some(Effect::Quit))
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel::<Local>();
        let got = rt.block_on(async {
            let task = tokio::spawn(async { panic!("engine work does not panic") });
            joined(task, &tx, "refresh").await
        });
        assert_eq!(got, None);
        match rx.try_recv() {
            Ok(Local::Fatal(text)) => assert!(text.starts_with("refresh failed: "), "{text}"),
            other => panic!("expected Fatal, got {other:?}"),
        }
        let ok = rt.block_on(async {
            let task = tokio::spawn(async { 7 });
            joined(task, &tx, "refresh").await
        });
        assert_eq!(ok, Some(7));
        assert!(rx.try_recv().is_err(), "a successful join sends nothing");
    }

    #[test]
    fn run_ui_stores_the_keymap_table_into_the_app() {
        let keys = [("quit", &["x"][..])]
            .into_iter()
            .map(|(n, s)| {
                (
                    n.to_owned(),
                    lastcall_engine::config::KeySpecs::Many(
                        s.iter().map(|s| (*s).to_owned()).collect(),
                    ),
                )
            })
            .collect();
        let keymap = Keymap::from_config(&keys).unwrap();
        let ui = Ui::new(App::new(), keymap.clone());
        assert_eq!(ui.app.keymap, keymap.table());
        assert_eq!(ui.app.keys_for("quit"), ["x".to_owned()]);
        assert!(ui.hits.is_none(), "no frame yet");
    }

    #[test]
    fn run_press_resolves_through_the_hit_map_and_resize_invalidates_it() {
        let mut ui = ui();
        let beta = Target::NavRoot(root("beta"));
        // Before any render a press hits nothing.
        assert_eq!(
            ui.event(&mouse(MouseEventKind::Down(MouseButton::Left), 5, 5)),
            (Changed::No, None)
        );
        assert_eq!(ui.app.selection, None);

        render_into(&mut ui);
        let (x, y) = target_center(&ui, &beta);
        assert_eq!(
            ui.event(&mouse(MouseEventKind::Down(MouseButton::Left), x, y))
                .0,
            Changed::Yes
        );
        assert_eq!(ui.app.selection, Some(Selection::Root(root("beta"))));

        // A resize drops the map: the same press is ignored until the next render.
        assert_eq!(ui.event(&Event::Resize(100, 30)).0, Changed::Yes);
        assert!(ui.hits.is_none());
        let before = ui.app.clone();
        let alpha = (5, 2); // alpha's repo row in the previous frame
        assert_eq!(
            ui.event(&mouse(
                MouseEventKind::Down(MouseButton::Left),
                alpha.0,
                alpha.1
            )),
            (Changed::No, None)
        );
        assert_eq!(ui.app, before);

        render_into(&mut ui);
        let (x, y) = target_center(&ui, &Target::NavRoot(root("alpha")));
        ui.event(&mouse(MouseEventKind::Down(MouseButton::Left), x, y));
        assert_eq!(ui.app.selection, Some(Selection::Root(root("alpha"))));
    }

    #[test]
    fn run_wheel_scrolls_the_pane_under_the_pointer() {
        let mut ui = ui();
        ui.app.apply(pile_event("alpha", alpha_two_hunks()));
        ui.app.select(Some(row("alpha", "f1")));
        ui.app.handle(Action::Open);
        assert_eq!(ui.app.focus, Focus::Diff);
        render_into(&mut ui);
        let hits = ui.hits.clone().unwrap();
        let nav = hits.nav.unwrap();
        let main = hits.main.unwrap();

        // Over the nav the selection moves even though the diff has focus.
        let (x, y) = (nav.x + 1, nav.y + 1);
        assert_eq!(
            ui.event(&mouse(MouseEventKind::ScrollDown, x, y)).0,
            Changed::Yes
        );
        assert_eq!(ui.app.selection, Some(row("alpha", "f2")));
        assert_eq!(ui.app.focus, Focus::Diff, "focus untouched");
        ui.event(&mouse(MouseEventKind::ScrollUp, x, y));
        assert_eq!(ui.app.selection, Some(row("alpha", "f1")));

        // Over the diff it scrolls the diff by WHEEL_LINES.
        let (x, y) = (main.x + 1, main.y + 1);
        assert_eq!(ui.app.diff.scroll, 0);
        assert_eq!(
            ui.event(&mouse(MouseEventKind::ScrollDown, x, y)).0,
            Changed::Yes
        );
        assert_eq!(
            ui.app.diff.scroll,
            usize::from(crate::tui::input::WHEEL_LINES)
        );
        assert_eq!(
            ui.app.selection,
            Some(row("alpha", "f1")),
            "selection untouched"
        );
        ui.event(&mouse(MouseEventKind::ScrollUp, x, y));
        assert_eq!(ui.app.diff.scroll, 0);

        // Without a hit map (after a resize) the wheel falls back to the app's own rule.
        ui.event(&Event::Resize(100, 30));
        ui.event(&mouse(MouseEventKind::ScrollDown, nav.x + 1, nav.y + 1));
        assert_eq!(ui.app.diff.scroll, 3, "no map: scrolls the diff");
        assert_eq!(ui.app.selection, Some(row("alpha", "f1")));
    }

    #[test]
    fn run_wheel_closes_help_like_any_other_action() {
        let mut ui = ui();
        render_into(&mut ui);
        ui.event(&key(KeyCode::Char('?')));
        assert!(ui.app.help);
        let nav = ui.hits.as_ref().unwrap().nav.unwrap();
        assert_eq!(
            ui.event(&mouse(MouseEventKind::ScrollDown, nav.x + 1, nav.y + 1))
                .0,
            Changed::Yes
        );
        assert!(!ui.app.help);
        assert_eq!(ui.app.selection, None, "the closing gesture is consumed");
    }

    #[test]
    fn run_keys_and_quit_flow_through_the_keymap() {
        let mut ui = ui();
        assert_eq!(ui.event(&key(KeyCode::Down)).0, Changed::Yes);
        assert_eq!(ui.app.selection, Some(Selection::Root(root("alpha"))));
        assert_eq!(
            ui.event(&key(KeyCode::Char('Z'))),
            (Changed::No, None),
            "unbound"
        );
        assert_eq!(ui.event(&Event::FocusGained), (Changed::No, None));
        assert_eq!(
            ui.event(&key(KeyCode::Char('q'))),
            (Changed::No, Some(Effect::Quit))
        );
        assert_eq!(
            ui.event(&key(KeyCode::Char('r'))),
            (Changed::Yes, Some(Effect::Refresh))
        );
    }

    /// Inside the confirm modal `q` and ctrl-c still quit (the Phase 3 ruling: they quit
    /// by default, everywhere), `Esc` only cancels, and every other key is swallowed.
    #[test]
    fn run_quit_keys_quit_from_inside_the_modal_and_esc_only_cancels() {
        let mut ui = ui();
        ui.app.apply(pile_event("alpha", rows_n(11, 0, 0)));
        ui.event(&key(KeyCode::Char('a')));
        assert_eq!(ui.app.selection, None, "`a` with nothing selected: nothing");
        assert_eq!(
            ui.event(&Event::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL
            ))),
            (Changed::Yes, None)
        );
        assert!(ui.app.confirm.is_some(), "11 files ask first");
        let open = ui.app.clone();
        for ev in [
            key(KeyCode::Char('j')),
            key(KeyCode::Char('a')),
            key(KeyCode::Char('?')),
            key(KeyCode::Char('r')),
            key(KeyCode::Char('h')),
        ] {
            assert_eq!(ui.event(&ev), (Changed::No, None), "{ev:?} swallowed");
            assert_eq!(ui.app, open);
        }
        assert_eq!(
            ui.event(&key(KeyCode::Char('q'))),
            (Changed::No, Some(Effect::Quit)),
            "q quits from inside the modal"
        );
        assert_eq!(
            ui.event(&Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL
            ))),
            (Changed::No, Some(Effect::Quit)),
            "ctrl-c quits from inside the modal"
        );
        assert_eq!(ui.app, open, "asking to quit changes nothing");
        assert_eq!(
            ui.event(&key(KeyCode::Esc)),
            (Changed::Yes, None),
            "Esc cancels, never quits"
        );
        assert!(ui.app.confirm.is_none());
        assert!(ui.app.accepting.is_none());
    }

    /// Under the modal the mouse is dropped before the app: the wheel over the nav (which
    /// otherwise reaches `move_selection` directly), a press on a nav row, a drag and a
    /// release all leave the app untouched; a resize still goes through and still
    /// invalidates the hit map.
    #[test]
    fn run_mouse_is_dropped_under_the_modal_but_resize_passes() {
        let mut ui = ui();
        ui.app.apply(pile_event("alpha", rows_n(11, 0, 0)));
        ui.app.select(Some(Selection::Root(root("alpha"))));
        render_into(&mut ui);
        let nav = ui.hits.as_ref().unwrap().nav.unwrap();
        let main = ui.hits.as_ref().unwrap().main.unwrap();
        assert_eq!(ui.event(&key(KeyCode::Char('a'))), (Changed::Yes, None));
        assert!(ui.app.confirm.is_some(), "11 files ask first");
        let open = ui.app.clone();
        let (nx, ny) = (nav.x + 1, nav.y + 1);
        for ev in [
            mouse(MouseEventKind::ScrollDown, nx, ny),
            mouse(MouseEventKind::ScrollUp, nx, ny),
            mouse(MouseEventKind::ScrollDown, main.x + 1, main.y + 1),
            mouse(MouseEventKind::Down(MouseButton::Left), nx, ny + 1),
            mouse(MouseEventKind::Drag(MouseButton::Left), nx + 3, ny + 1),
            mouse(MouseEventKind::Up(MouseButton::Left), nx + 3, ny + 1),
            Event::FocusGained,
        ] {
            assert_eq!(ui.event(&ev), (Changed::No, None), "{ev:?} dropped");
            assert_eq!(ui.app, open, "{ev:?} touched the app");
        }
        assert!(ui.hits.is_some(), "the hit map survives dropped events");
        assert_eq!(ui.event(&Event::Resize(120, 40)), (Changed::Yes, None));
        assert!(ui.hits.is_none(), "a resize still invalidates the hit map");
        assert_eq!(ui.app.size, (120, 40));
        assert!(ui.app.confirm.is_some(), "the modal is still open");
    }

    #[test]
    fn run_local_results_feed_the_app() {
        let mut ui = Ui::new(App::new(), Keymap::defaults());
        // Roots arrive (the seed, or a SyncRoots): nothing listed yet.
        assert_eq!(
            ui.local(Local::Roots(vec![
                meta("alpha"),
                meta("beta"),
                meta("notes")
            ])),
            (Changed::Yes, None)
        );
        assert_eq!(ui.app.roots.len(), 3);
        assert!(ui.app.listed_roots().next().is_none());
        // A refresh: piles, a failed root, then done.
        assert_eq!(ui.event(&key(KeyCode::Char('r'))).1, Some(Effect::Refresh));
        assert!(ui.app.refreshing);
        assert_eq!(
            ui.local(Local::Pile(root("alpha"), 1, pile("alpha"))),
            (Changed::Yes, None)
        );
        assert!(ui.app.roots[&root("alpha")].listed());
        assert_eq!(
            ui.local(Local::Pile(root("alpha"), 1, pile("alpha"))),
            (Changed::No, None),
            "an unchanged pile is no change"
        );
        assert_eq!(
            ui.local(Local::Notice(
                Some(root("beta")),
                "scan failed: boom".into()
            )),
            (Changed::Yes, None)
        );
        assert_eq!(
            ui.app.status.as_ref().unwrap().text,
            "beta: scan failed: boom"
        );
        assert_eq!(ui.local(Local::RefreshDone), (Changed::Yes, None));
        assert!(!ui.app.refreshing);
        assert_eq!(ui.app.status.as_ref().unwrap().text, "refreshed");
        assert_eq!(
            ui.local(Local::RefreshDone),
            (Changed::No, None),
            "a stray done is nothing"
        );
        // A watcher event goes straight to apply; Head asks for a SyncRoots.
        let (changed, effect) = ui.engine(EngineEvent::Head {
            root: root("alpha"),
            from: None,
            to: None,
            branch: Some("main".into()),
            notice: Some("committed on main (1 commit)".into()),
        });
        assert_eq!((changed, effect), (Changed::Yes, Some(Effect::SyncRoots)));
    }

    #[test]
    fn run_shutdown_restores_before_joining_before_runtime_shutdown() {
        #[derive(Default)]
        struct Recorder(Vec<&'static str>);
        impl Shutdown for Recorder {
            fn restore(&mut self) {
                self.0.push("restore");
            }
            fn shutdown_herdr(&mut self) {
                self.0.push("herdr");
            }
            fn join_watcher(&mut self) {
                self.0.push("join");
            }
            fn shutdown_runtime(&mut self) {
                self.0.push("shutdown");
            }
        }
        let mut r = Recorder::default();
        shut_down(&mut r);
        assert_eq!(r.0, ["restore", "herdr", "join", "shutdown"]);
    }

    #[test]
    fn run_real_shutdown_is_safe_with_nothing_to_shut_down() {
        // The terminal was never entered here, so `restore` must be a no-op and the
        // absent watcher/runtime must be skipped rather than panicked on.
        let mut real = RealShutdown {
            terminal: None,
            guard: None,
            runtime: None,
            watcher: None,
            herdr: None,
        };
        shut_down(&mut real);
        assert!(!term::is_active());
        assert_eq!(SHUTDOWN_BUDGET, Duration::from_millis(500));
        assert!(INPUT_POLL <= Duration::from_millis(50));
    }

    #[test]
    fn run_bounded_join_works_from_outside_the_runtime_and_gives_up_on_time() {
        // Regression: the first pty run panicked with "no reactor running" because the
        // timeout was built before `block_on` entered the runtime. The helper is called
        // exactly as `RealShutdown::join_watcher` calls it: from a plain thread.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        assert_eq!(
            block_bounded(&rt, Duration::from_millis(20), async { 7 }),
            Some(7)
        );
        let started = std::time::Instant::now();
        let hung = block_bounded(&rt, Duration::from_millis(20), std::future::pending::<()>());
        assert_eq!(hung, None);
        assert!(started.elapsed() < Duration::from_secs(5));
        rt.shutdown_timeout(Duration::from_millis(20));
    }
}
