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
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, MouseButton, MouseEvent, MouseEventKind,
};
use lastcall_engine::engine::{AcceptRequest, Engine, RenderedHunk, RestoreRequest};
use lastcall_engine::env::Env;
use lastcall_engine::herdr::HerdrEvent;
use lastcall_engine::herdr::client::ClientHandle;
use lastcall_engine::herdr::transport::SocketTransport;
use lastcall_engine::hunks::Expanded;
use lastcall_engine::scan::{Pile, Row};
use lastcall_engine::watcher::{EngineEvent, EngineTimings, Watcher, blocking};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use super::app::{
    AcceptFailed, AcceptResult, App, Changed, Effect, FlagResult, RestoreResult, RootMeta,
};
use super::herdr::{self, HerdrLink, HerdrPlan, HerdrUpdate, Link, ToastMsg};
use super::input::{
    Action, Key, Keymap, modal_action, note_action, pick_action, pointer, to_action,
};
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
    /// An `Effect::Restore` finished. Shaped like `Accepted` though a restore covers one
    /// root, so the two reducers read the same way.
    Restored(Vec<(PathBuf, RestoreResult)>),
    /// An `Effect::Flag` or `Effect::Unflag` finished: the root it covered and the ledger
    /// write's answer. One root, never a list — a flag is always one path.
    Flagged(PathBuf, FlagResult),
    /// An `Effect::Stage` finished: the agent's label and whether `pane.send_text` landed.
    Staged(String, Result<(), String>),
    /// An `Effect::Export` finished: the file the export was appended to, or why not.
    Exported(Result<PathBuf, String>),
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
    /// An `Effect::Expand` finished: one collapsed row's on-demand hunks (deliverable 4).
    /// The row `hunks_of` was given travels back with the answer, so the app can tell an
    /// answer for the oids on screen from one for oids a pile has since replaced.
    Expanded(PathBuf, Box<Row>, Expanded),
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
        // The note modal is a text field: every printable key is content, so it is
        // consulted *before* the keymap and swallows it whole — only the keymap's `quit`
        // keys survive, and only in their non-printable form (`note_action`'s `quit_only`,
        // so `q` types a q). `Event::Paste` is why bracketed paste is on while the modal
        // lives: a pasted traceback arrives as one insert rather than a key storm.
        if self.app.note.is_some() {
            match event {
                Event::Key(_) | Event::Paste(_) => {
                    return match note_action(event, &self.keymap) {
                        Some(action) => self.app.handle(action),
                        None => (Changed::No, None),
                    };
                }
                // A resize still reaches the app below (and invalidates the hit map); the
                // mouse is dropped, so a click behind the modal cannot move the selection.
                Event::Resize(..) => {}
                _ => return (Changed::No, None),
            }
        }
        // The picker is a list, not a field: arrows and `j`/`k` move, Enter sends, Esc
        // cancels, the keymap's `quit` keys quit, everything else is swallowed.
        if self.app.picker.is_some() {
            match event {
                Event::Key(_) => {
                    return match pick_action(event, &self.keymap) {
                        Some(action) => self.app.handle(action),
                        None => (Changed::No, None),
                    };
                }
                Event::Resize(..) => {}
                _ => return (Changed::No, None),
            }
        }
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
            Local::Restored(results) => (self.app.restored(results), None),
            Local::Flagged(root, flagged) => self.app.flagged(root, flagged),
            Local::Staged(label, result) => (self.app.staged(label, result), None),
            Local::Exported(result) => (self.app.exported(result), None),
            Local::Roots(metas) => (self.app.sync_roots(metas), None),
            Local::RefreshDone => (self.app.refresh_done(), None),
            Local::Notice(root, text) => self.app.apply(EngineEvent::Notice { root, text }),
            Local::Fatal(text) => {
                self.app.set_status(text);
                (Changed::No, Some(Effect::Quit))
            }
            Local::Herdr(update) => self.app.handle(Action::Herdr(update)),
            Local::Expanded(root, row, view) => (self.app.set_expanded(root, &row, view), None),
        }
    }

    /// Record the hit map a render produced.
    pub fn rendered(&mut self, hits: HitMap) {
        // Deliverable 9: only a frame that actually drew the nav has an offset to report.
        // A `None` means the nav was not on screen (below `NAV_MIN_COLS`), and the app keeps
        // the offset it had, so widening the window returns the reader where they were.
        if let Some(top) = hits.nav_top {
            self.app.nav_top = top;
        }
        self.hits = Some(hits);
    }
}

/// Round-robin **rounds** one pass drains beyond the event the `select!` woke on; a round
/// polls each of the four sources once, so a pass folds at most four times this many
/// events. A pass that keeps folding forever is a pass that never draws, so the drain stops
/// here whatever is still queued — the next iteration picks the rest up (deliverable 6).
pub const DRAIN_CAP: usize = 256;

/// Why a pass ended the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// `Effect::Quit`: the user asked to leave.
    Quit,
    /// `Local::Fatal`: an engine task died; the panic hook already restored the terminal.
    Fatal(String),
}

/// One turn of the loop: everything folded since the last frame, and what it asks for.
/// The loop runs `effects` in order, breaks on `stop`, and draws **once** when
/// `changed == Yes` — not once per event (deliverable 6).
#[derive(Debug, Default, PartialEq)]
pub struct Pass {
    pub changed: Changed,
    /// The first source that made this pass worth drawing; the `cause=` of the `draw` probe.
    pub cause: Option<&'static str>,
    pub effects: Vec<Effect>,
    pub stop: Option<Stop>,
    /// A herdr `worktree.*` event was drained in this pass. The loop **arms** the
    /// `WORKTREE_DEBOUNCE` timer for it exactly as the `select!` arm does for the event it
    /// woke on — never a rescan on the spot, which would bypass the Phase 5 debounce and
    /// cancel a timer an earlier event of the same burst had armed (verifier (b) F2).
    pub worktree: bool,
    /// A mouse press the drain refused to fold because the pass already has something to
    /// draw. It is folded at the top of the next iteration, **after** the frame, so a press
    /// always resolves against the hit map of a frame the user actually saw.
    pub held: Option<Event>,
}

impl Pass {
    /// A pass seeded with what the `select!` arm folded. It goes through [`Pass::fold`] so
    /// the seed obeys every rule the drained events do — in particular a `q` that arrives as
    /// the pass's *first* event is a `Stop::Quit`, not an effect for the dispatch loop.
    fn of(source: &'static str, changed: Changed, effect: Option<Effect>) -> Self {
        let mut pass = Self::default();
        pass.fold(source, (changed, effect));
        pass
    }

    /// Fold one event's outcome in. `source` is one of `input`, `engine`, `local`, `herdr`,
    /// `timer`, `tick`: deliverable 8's `fold source= changed=` probe, and the `cause=` the
    /// draw that follows reports — the first source that made the pass worth drawing, which
    /// is the question "why did the screen just repaint?" answers with.
    fn fold(&mut self, source: &'static str, (changed, effect): (Changed, Option<Effect>)) {
        tracing::debug!(source, changed = ?changed, "fold");
        if changed == Changed::Yes && self.cause.is_none() {
            self.cause = Some(source);
        }
        self.changed = self.changed.or(changed);
        match effect {
            Some(Effect::Quit) => self.stop = Some(Stop::Quit),
            Some(other) => self.effects.push(other),
            None => {}
        }
    }
}

/// The loop's event queues, by mutable reference so a test can build them with
/// `mpsc::unbounded_channel` / `mpsc::channel` and drive [`drain`] without a `Watcher`,
/// a terminal or a runtime.
pub(crate) struct Sources<'a> {
    pub engine: &'a mut mpsc::Receiver<EngineEvent>,
    pub input: &'a mut mpsc::UnboundedReceiver<Event>,
    pub local: &'a mut mpsc::UnboundedReceiver<Local>,
}

/// Fold every event already queued into `pass`, up to [`DRAIN_CAP`], so a burst of piles or
/// a wheel spin costs one frame instead of one frame each.
///
/// The rules, in the order they matter:
///
/// - `Effect::Quit` and `Local::Fatal` end the drain at once; whatever is still queued is
///   never folded, because the loop is leaving.
/// - A mouse `Press` ends the drain **once the pass has something to draw**, and is handed
///   back in `Pass::held` rather than folded: a press resolves through the hit map of the
///   last drawn frame, so folding it behind an undrawn change would resolve it against a
///   frame nobody saw. Every other input event — keys, the wheel, resizes — folds freely.
/// - Sources are polled round-robin and the drain ends when a whole round is empty.
pub(crate) fn drain(ui: &mut Ui, sources: &mut Sources<'_>, link: &mut Herdr, pass: &mut Pass) {
    for _ in 0..DRAIN_CAP {
        if pass.stop.is_some() || pass.held.is_some() {
            return;
        }
        let mut any = false;
        if let Ok(event) = sources.input.try_recv() {
            any = true;
            if is_press(&event) && pass.changed == Changed::Yes {
                pass.held = Some(event);
                return;
            }
            pass.fold("input", ui.event(&event));
        }
        if pass.stop.is_none()
            && let Ok(event) = sources.engine.try_recv()
        {
            any = true;
            pass.fold("engine", ui.engine(event));
        }
        if pass.stop.is_none()
            && let Ok(local) = sources.local.try_recv()
        {
            any = true;
            match local {
                Local::Fatal(text) => {
                    pass.stop = Some(Stop::Fatal(text));
                    return;
                }
                Local::Roots(metas) => {
                    pass.fold("local", ui.local(Local::Roots(metas)));
                    pass.fold("local", herdr_rederive(ui, link));
                }
                other => pass.fold("local", ui.local(other)),
            }
        }
        if pass.stop.is_none()
            && let Some(rx) = link.events.as_mut()
            && let Ok(event) = rx.try_recv()
        {
            any = true;
            pass.worktree |= herdr::triggers_rescan(&event);
            if let Some(update) = herdr::update_of(&event) {
                pass.fold("herdr", ui.app.handle(Action::Herdr(update)));
            }
            if herdr::rederives(&event) {
                pass.fold("herdr", herdr_rederive(ui, link));
            }
        }
        if !any {
            return;
        }
    }
}

fn is_press(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            ..
        })
    )
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

/// `Effect::Restore`: the working-tree write, off the UI task, in one `blocking` closure so
/// the op and its rescan are one critical section — the same shape as `spawn_accept`, and
/// the only place in the TUI that reaches `Engine::restore`.
fn spawn_restore(
    engine: &Arc<Mutex<Engine>>,
    tx: mpsc::UnboundedSender<Local>,
    reqs: Vec<(PathBuf, RestoreRequest)>,
) {
    let engine = engine.clone();
    tokio::spawn(async move {
        let restore = tokio::spawn(async move {
            blocking(&engine, move |e| {
                reqs.into_iter()
                    .map(|(root, req)| {
                        let result = e.restore(&root, req).map_err(|e| AcceptFailed::of(&e));
                        (root, result)
                    })
                    .collect::<Vec<_>>()
            })
            .await
        });
        if let Some(results) = joined(restore, &tx, "restore").await {
            let _ = tx.send(Local::Restored(results));
        }
    });
}

/// `Effect::Flag`: the ledger write and its rescan in one `blocking` closure, the same
/// shape as `spawn_accept`. The engine renders the export (only it has the flag's
/// `created_at`), so the answer carries the paste-ready text the send will use.
fn spawn_flag(
    engine: &Arc<Mutex<Engine>>,
    tx: mpsc::UnboundedSender<Local>,
    root: PathBuf,
    path: Vec<u8>,
    note: String,
    hunk: Option<RenderedHunk>,
) {
    let engine = engine.clone();
    let back = root.clone();
    tokio::spawn(async move {
        let task = tokio::spawn(async move {
            blocking(&engine, move |e| {
                e.flag(&root, &path, &note, hunk)
                    .map_err(|e| AcceptFailed::of(&e))
            })
            .await
        });
        if let Some(result) = joined(task, &tx, "flag").await {
            let _ = tx.send(Local::Flagged(back, result));
        }
    });
}

/// `Effect::Unflag`: [`Engine::unflag`] under the same lock discipline. Its answer is a
/// `Local::Flagged` too — an unflag is a flag write with an empty export, and `App::flagged`
/// tells them apart by whether a send was pending.
fn spawn_unflag(
    engine: &Arc<Mutex<Engine>>,
    tx: mpsc::UnboundedSender<Local>,
    root: PathBuf,
    path: Vec<u8>,
) {
    let engine = engine.clone();
    let back = root.clone();
    tokio::spawn(async move {
        let task = tokio::spawn(async move {
            blocking(&engine, move |e| {
                e.unflag(&root, &path).map_err(|e| AcceptFailed::of(&e))
            })
            .await
        });
        if let Some(result) = joined(task, &tx, "unflag").await {
            let _ = tx.send(Local::Flagged(back, result));
        }
    });
}

/// `Effect::Stage`: `pane.send_text` the export into one agent pane, bracketed-paste
/// wrapped by [`herdr::stage`] so it lands unsubmitted. No link means no send: the flag is
/// already on disk, and `App::staged` says so.
fn spawn_stage(
    transport: Option<SocketTransport>,
    tx: mpsc::UnboundedSender<Local>,
    pane_id: String,
    label: String,
    export: String,
) {
    tokio::spawn(async move {
        let result = match transport {
            Some(t) => herdr::stage(&t, &pane_id, &export).await,
            None => Err("no herdr link".to_owned()),
        };
        let _ = tx.send(Local::Staged(label, result));
    });
}

/// `Effect::Export`: append the export to `<state_dir>/exports/<root basename>/<date>.md`.
///
/// The one file the TUI writes, and the only writer of it — no worktree file is ever opened
/// for writing here. Append, never truncate: a day's flags on one root accumulate in one
/// file, each separated by a blank line, so the fallback reads as a log rather than as the
/// last thing that happened.
fn spawn_export(
    tx: mpsc::UnboundedSender<Local>,
    state_dir: PathBuf,
    date: String,
    root: PathBuf,
    export: String,
) {
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            let dir = state_dir.join("exports").join(export_dir_name(&root));
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let path = dir.join(format!("{date}.md"));
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| e.to_string())?;
            let mut body = export;
            if !body.ends_with('\n') {
                body.push('\n');
            }
            body.push('\n');
            f.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
            Ok(path)
        })
        .await;
        let result = match result {
            Ok(r) => r,
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(Local::Exported(result));
    });
}

/// The export file's directory name: the root's basename, or `root` for a filesystem root
/// with none. A basename that is not UTF-8 is `to_string_lossy`'d rather than refused — the
/// name is a label for a human, not a key.
/// The export file's date stamp: the engine's own clock, ISO-8601, cut at the day. Reading
/// it from the engine's clock rather than `SystemTime::now()` is what lets a test with a
/// `FixedClock` name the file it expects.
fn export_date(clock: &Arc<dyn lastcall_engine::ledger::Clock + Send + Sync>) -> String {
    clock.now_iso8601().chars().take(10).collect()
}

fn export_dir_name(root: &std::path::Path) -> String {
    match root.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => "root".to_owned(),
    }
}

/// `Effect::Expand`: one collapsed row's hunks off the UI task (deliverable 4). The row
/// travels with the request, so the diff is computed from the oids the screen was showing;
/// a failure is a notice, never a fatal — the row is still there to accept whole.
fn spawn_expand(
    engine: &Arc<Mutex<Engine>>,
    tx: mpsc::UnboundedSender<Local>,
    root: PathBuf,
    row: Box<Row>,
) {
    let engine = engine.clone();
    tokio::spawn(async move {
        let task = tokio::spawn(async move {
            blocking(&engine, move |e| {
                let view = e.hunks_of(&root, &row);
                (root, row, view)
            })
            .await
        });
        let Some((root, row, view)) = joined(task, &tx, "expand").await else {
            return;
        };
        let local = match view {
            Ok(view) => Local::Expanded(root, row, view),
            Err(e) => Local::Notice(Some(root), format!("expand failed: {e}")),
        };
        let _ = tx.send(local);
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
pub(crate) struct Herdr {
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

/// The connect arm's future: the outcome of [`herdr::connect`] **once**, then forever, so
/// an adopted link never re-enters the arm. Cancel-safe — a sibling arm winning leaves the
/// task running and the next poll picks it up where it was. `None` is a task that panicked
/// or was aborted: the badge stays as it is.
async fn connect_ready(
    task: &mut Option<tokio::task::JoinHandle<Result<HerdrLink, Link>>>,
) -> Option<Result<HerdrLink, Link>> {
    let Some(handle) = task.as_mut() else {
        return std::future::pending().await;
    };
    let joined = (&mut *handle).await;
    *task = None;
    match joined {
        Ok(outcome) => Some(outcome),
        Err(e) => {
            tracing::warn!(error = %e, "the herdr connect task ended without an answer");
            None
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

    // Copied out before the engine is moved into its watcher: both are immutable for the
    // life of the process, and the export path is resolved without taking the engine lock.
    let state_dir = engine.layout().state_dir().to_path_buf();
    let clock = engine.options().clock.clone();

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

            // The link is opened after the first frame, and on its **own task** (review (b)
            // F3): discovery and the protocol guard are bounded, but a socket that accepts
            // and never answers holds them for seconds, and keys, resizes, SIGINT and the
            // watcher have to keep being served throughout. The result arrives through the
            // loop's own arm, like everything else.
            ui.app.herdr.scoped = plan.scoped;
            ui.app.herdr.toast = plan.toast;
            let mut connecting = Some(tokio::spawn(async move {
                herdr::connect(&env, plan).await
            }));

            let mut tick = tokio::time::interval(TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // the immediate first tick
            let mut fatal: Option<String> = None;
            // Set by a `worktree.*` event, fired once the burst has been quiet this long.
            let mut worktree_due: Option<tokio::time::Instant> = None;
            // A press the previous pass refused to fold (see [`drain`]): folded here,
            // after that pass drew, so it resolves against the frame the user saw.
            let mut held: Option<Event> = None;
            // Bracketed paste is on only while the note modal lives. It is a terminal mode,
            // not an app mode, so it is toggled here rather than through an `Effect`: the
            // modal can close by sending, by cancelling or by quitting, and one comparison
            // after every pass covers all three without a variant per exit.
            let mut paste_on = false;
            loop {
                // Copied out so the timer future borrows nothing a handler assigns to.
                let due = worktree_due;
                let mut rescan = false;
                let (source, (changed, effect)) = match held.take() {
                    Some(event) => ("input", ui.event(&event)),
                    None => tokio::select! {
                    _ = signals.recv() => break,
                    event = watcher.events.recv() => ("engine", match event {
                        Some(event) => ui.engine(event),
                        None => {
                            tracing::warn!("watcher ended; exiting");
                            ui.app.set_status("watcher ended");
                            draw(&mut terminal, &mut ui)?;
                            break;
                        }
                    }),
                    event = input_rx.recv() => ("input", match event {
                        Some(event) => ui.event(&event),
                        None => break, // the reader thread died
                    }),
                    local = local_rx.recv() => ("local", match local {
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
                    }),
                    event = herdr_recv(&mut link.events) => ("herdr", match event {
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
                    }),
                    opened = connect_ready(&mut connecting) => ("herdr", match opened {
                        // The badge itself comes later, with the client's `Connected`.
                        Some(Ok(open)) => {
                            link = Herdr::adopt(open, local_tx.clone());
                            (Changed::No, None)
                        }
                        Some(Err(badge)) => {
                            let changed = if badge == Link::Off { Changed::No } else { Changed::Yes };
                            ui.app.herdr.link = badge;
                            (changed, None)
                        }
                        None => (Changed::No, None),
                    }),
                    _ = async {
                        match due {
                            Some(at) => tokio::time::sleep_until(at).await,
                            None => std::future::pending().await,
                        }
                    } => {
                        rescan = true;
                        ("timer", (Changed::No, None))
                    }
                    _ = tick.tick() => ("tick", ui.app.handle(Action::Tick)),
                    },
                };
                // Everything else already queued joins this pass, so a burst of piles or a
                // wheel spin costs one frame rather than one frame each (deliverable 6).
                let mut pass = Pass::of(source, changed, effect);
                {
                    let mut sources = Sources {
                        engine: &mut watcher.events,
                        input: &mut input_rx,
                        local: &mut local_rx,
                    };
                    drain(&mut ui, &mut sources, &mut link, &mut pass);
                }
                held = pass.held.take();
                if rescan {
                    // Deliverable 7: the event is only a trigger — `roots::discover` decides
                    // what is a root, and `RootsChanged` + the new pile take the usual path.
                    worktree_due = None;
                    watcher.request_rescan();
                }
                if pass.worktree {
                    // A drained `worktree.*` event (re)arms the debounce, after the timer
                    // check above so an event that shares a pass with the timer firing
                    // opens the next window rather than being folded into the old one.
                    worktree_due = Some(tokio::time::Instant::now() + WORKTREE_DEBOUNCE);
                }
                let mut stop = pass.stop;
                for effect in pass.effects {
                    match effect {
                        // `Pass::of` and `Pass::fold` both route a quit into `Pass::stop`, so
                        // this arm is belt and braces — and never a `break`, which would only
                        // leave this `for` and drop the rest of the pass's effects.
                        Effect::Quit => stop = Some(Stop::Quit),
                        Effect::Refresh => spawn_refresh(&watcher.engine, local_tx.clone()),
                        Effect::SyncRoots => spawn_sync_roots(&watcher.engine, local_tx.clone()),
                        Effect::Accept(reqs) => {
                            spawn_accept(&watcher.engine, local_tx.clone(), reqs)
                        }
                        Effect::Restore(reqs) => {
                            spawn_restore(&watcher.engine, local_tx.clone(), reqs)
                        }
                        Effect::Flag {
                            root,
                            path,
                            note,
                            hunk,
                        } => spawn_flag(
                            &watcher.engine,
                            local_tx.clone(),
                            root,
                            path,
                            note,
                            hunk,
                        ),
                        Effect::Unflag { root, path } => {
                            spawn_unflag(&watcher.engine, local_tx.clone(), root, path)
                        }
                        Effect::Stage {
                            pane_id,
                            label,
                            export,
                        } => spawn_stage(
                            link.transport.clone(),
                            local_tx.clone(),
                            pane_id,
                            label,
                            export,
                        ),
                        Effect::Export { root, export } => spawn_export(
                            local_tx.clone(),
                            state_dir.clone(),
                            export_date(&clock),
                            root,
                            export,
                        ),
                        Effect::Expand(root, row) => {
                            spawn_expand(&watcher.engine, local_tx.clone(), root, row)
                        }
                        Effect::Focus(pane) => {
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
                        Effect::Toast(request) => {
                            if let Some(tx) = &link.toast {
                                for (root, name) in request.ready {
                                    let _ = tx.send(ToastMsg::Ready { root, name });
                                }
                                for root in request.dropped {
                                    let _ = tx.send(ToastMsg::Drop(root));
                                }
                            }
                        }
                    }
                }
                match stop {
                    Some(Stop::Quit) => break,
                    Some(Stop::Fatal(text)) => {
                        fatal = Some(text);
                        break;
                    }
                    None => {}
                }
                if ui.app.note.is_some() != paste_on {
                    paste_on = ui.app.note.is_some();
                    // Before the draw, so the frame that first shows the modal is already
                    // able to receive a paste. A terminal that does not support the mode
                    // ignores the sequence; a write that fails is not worth ending on.
                    let _ = if paste_on {
                        crossterm::execute!(io::stdout(), EnableBracketedPaste)
                    } else {
                        crossterm::execute!(io::stdout(), DisableBracketedPaste)
                    };
                }
                if pass.changed == Changed::Yes {
                    // Deliverable 8: one line per repaint, saying why and how long. A
                    // `draw` per pile in a burst is the symptom deliverable 6 removed, and
                    // this is how the sponsor sees it stay removed.
                    let started = std::time::Instant::now();
                    draw(&mut terminal, &mut ui)?;
                    tracing::debug!(
                        cause = pass.cause.unwrap_or("unknown"),
                        ms = started.elapsed().as_millis() as u64,
                        "draw"
                    );
                }
            }
            // A connect still in flight has nothing left to deliver.
            if let Some(task) = connecting.take() {
                task.abort();
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

    /// Channels a `drain` test drives: the three loop queues plus the herdr link, none of
    /// which needs a `Watcher`, a terminal or a runtime.
    struct Wires {
        engine_tx: mpsc::Sender<EngineEvent>,
        engine_rx: mpsc::Receiver<EngineEvent>,
        input_tx: mpsc::UnboundedSender<Event>,
        input_rx: mpsc::UnboundedReceiver<Event>,
        local_tx: mpsc::UnboundedSender<Local>,
        local_rx: mpsc::UnboundedReceiver<Local>,
        link: Herdr,
    }

    impl Wires {
        fn new() -> Self {
            let (engine_tx, engine_rx) = mpsc::channel(512);
            let (input_tx, input_rx) = mpsc::unbounded_channel();
            let (local_tx, local_rx) = mpsc::unbounded_channel();
            Self {
                engine_tx,
                engine_rx,
                input_tx,
                input_rx,
                local_tx,
                local_rx,
                link: Herdr::default(),
            }
        }

        fn drain_into(&mut self, ui: &mut Ui, seed: (Changed, Option<Effect>)) -> Pass {
            let mut pass = Pass::of("select", seed.0, seed.1);
            let mut sources = Sources {
                engine: &mut self.engine_rx,
                input: &mut self.input_rx,
                local: &mut self.local_rx,
            };
            drain(ui, &mut sources, &mut self.link, &mut pass);
            pass
        }
    }

    /// Deliverable 9: `Ui::rendered` is where the frame's nav offset becomes the app's, and
    /// it writes back **only** what a frame that drew the nav reported. Below
    /// `NAV_MIN_COLS` there is no nav pane and therefore no offset, so a narrow window must
    /// not reset one the reader will see again when it widens.
    #[test]
    fn run_nav_offset_is_written_back_only_by_a_frame_that_drew_the_nav() {
        fn draw_at(ui: &mut Ui, w: u16, h: u16) -> Option<usize> {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut hits = HitMap::default();
            term.draw(|f| hits = render(&ui.app, f)).unwrap();
            let reported = hits.nav_top;
            ui.rendered(hits);
            reported
        }

        let mut app = App::new();
        app.sync_roots(vec![meta("alpha")]);
        app.apply(pile_event("alpha", rows_n(60, 0, 0)));
        app.handle(Action::Resize(100, 30));
        let mut ui = Ui::new(app, Keymap::defaults());

        // A selection deep in the list scrolls the nav and the offset lands on the app.
        ui.app.select(Some(row("alpha", "p50")));
        assert!(draw_at(&mut ui, 100, 30).is_some());
        let scrolled = ui.app.nav_top;
        assert!(scrolled > 0, "the nav scrolled to reach p50");

        // 60 columns is under `NAV_MIN_COLS`: no nav, nothing to report, nothing written.
        ui.app.handle(Action::Resize(60, 30));
        assert_eq!(draw_at(&mut ui, 60, 30), None, "no nav pane, no offset");
        assert_eq!(
            ui.app.nav_top, scrolled,
            "the offset survived the narrow frame"
        );

        // Wide again, and the reader is where they were.
        ui.app.handle(Action::Resize(100, 30));
        assert_eq!(draw_at(&mut ui, 100, 30), Some(scrolled));
        assert_eq!(ui.app.nav_top, scrolled);
    }

    /// Deliverable 6(a): a burst of piles is **one** pass and therefore one frame. Before
    /// the drain the loop drew once per `Changed::Yes`, so a rescan of many roots repainted
    /// the screen once per root.
    #[test]
    fn run_drain_folds_a_burst_of_piles_into_one_pass() {
        let mut ui = ui();
        let mut wires = Wires::new();
        // Both pile sources at once: the drain is round-robin, and the highest `seq` wins
        // whichever queue it arrived on.
        for seq in 1..=17u64 {
            let pile = alpha_hunks(seq as usize);
            if seq % 2 == 0 {
                wires
                    .engine_tx
                    .try_send(EngineEvent::Pile {
                        root: root("alpha"),
                        seq,
                        pile,
                    })
                    .unwrap();
            } else {
                wires
                    .local_tx
                    .send(Local::Pile(root("alpha"), seq, pile))
                    .unwrap();
            }
        }
        let pass = wires.drain_into(&mut ui, (Changed::No, None));
        assert_eq!(pass.changed, Changed::Yes);
        assert!(pass.stop.is_none() && pass.held.is_none());
        assert!(pass.effects.is_empty(), "{:?}", pass.effects);
        // The last pile won, and one render serves all seventeen.
        assert_eq!(ui.app.roots[&root("alpha")].pile.rows[0].hunks.len(), 17);
        render_into(&mut ui);
        // Nothing is left queued: the whole burst was folded.
        let after = wires.drain_into(&mut ui, (Changed::No, None));
        assert_eq!(after, Pass::default());
    }

    /// A press must resolve against a frame the user saw. The key ahead of it in the queue
    /// moves the page, so folding the press in the same pass would resolve it through the
    /// hit map of the **old** frame; the drain hands it back instead, and the loop folds it
    /// after drawing. The click then lands where the drawn frame says it does.
    #[test]
    fn run_drain_holds_a_press_behind_an_undrawn_change() {
        let mut ui = ui();
        render_into(&mut ui);
        ui.app.select(Some(row("alpha", "f1")));
        render_into(&mut ui);
        let (x, y) = target_center(&ui, &Target::NavRow(root("beta"), b"u1".to_vec()));

        let mut wires = Wires::new();
        wires.input_tx.send(key(KeyCode::PageDown)).unwrap();
        wires
            .input_tx
            .send(mouse(MouseEventKind::Down(MouseButton::Left), x, y))
            .unwrap();
        let pass = wires.drain_into(&mut ui, (Changed::No, None));
        assert_eq!(pass.changed, Changed::Yes, "the page key moved something");
        assert_eq!(
            pass.cause,
            Some("input"),
            "deliverable 8: the draw reports the source that earned it"
        );
        let held = pass.held.expect("the press stayed queued");
        assert!(is_press(&held));
        assert_ne!(
            ui.app.selection,
            Some(row("beta", "u1")),
            "the press has not been folded yet"
        );

        // The loop draws, then folds the held press against that frame.
        render_into(&mut ui);
        ui.event(&held);
        assert_eq!(ui.app.selection, Some(row("beta", "u1")));

        // A wheel event behind the same change folds freely: it needs no hit map.
        wires.input_tx.send(key(KeyCode::PageUp)).unwrap();
        wires
            .input_tx
            .send(mouse(MouseEventKind::ScrollDown, x, y))
            .unwrap();
        let pass = wires.drain_into(&mut ui, (Changed::No, None));
        assert!(pass.held.is_none(), "the wheel is not a press");
    }

    /// `Quit` and `Local::Fatal` end the drain where they are: whatever is behind them is
    /// never folded, because the loop is leaving.
    #[test]
    fn run_drain_stops_at_quit_and_at_a_fatal() {
        // The seed obeys the same rule: a `q` pressed with an empty queue never reaches the
        // effect dispatch, it *is* the stop.
        let seeded = Pass::of("input", Changed::No, Some(Effect::Quit));
        assert_eq!(seeded.stop, Some(Stop::Quit));
        assert!(seeded.effects.is_empty());
        {
            let mut ui = ui();
            let mut wires = Wires::new();
            wires.input_tx.send(key(KeyCode::Char('r'))).unwrap();
            wires.input_tx.send(key(KeyCode::Char('q'))).unwrap();
            for seq in 1..=5 {
                wires
                    .local_tx
                    .send(Local::Pile(root("alpha"), seq, alpha_hunks(seq as usize)))
                    .unwrap();
            }
            let pass = wires.drain_into(&mut ui, (Changed::No, None));
            assert_eq!(pass.stop, Some(Stop::Quit));
            assert!(
                matches!(pass.effects.as_slice(), [Effect::Refresh]),
                "the refresh ahead of the quit still runs: {:?}",
                pass.effects
            );
        }
        {
            let mut ui = ui();
            let mut wires = Wires::new();
            wires.local_tx.send(Local::RefreshDone).unwrap();
            wires
                .local_tx
                .send(Local::Fatal("accept failed: panic".to_owned()))
                .unwrap();
            wires
                .local_tx
                .send(Local::Pile(root("alpha"), 9, alpha_hunks(3)))
                .unwrap();
            let pass = wires.drain_into(&mut ui, (Changed::No, None));
            assert_eq!(
                pass.stop,
                Some(Stop::Fatal("accept failed: panic".to_owned()))
            );
            assert_eq!(
                ui.app.roots[&root("alpha")].pile.rows[0].hunks.len(),
                1,
                "the pile behind the fatal was never folded"
            );
        }
    }

    /// Verifier (b) F2: a `worktree.*` event that reaches the loop through the drain must
    /// take the same road as one that woke the `select!` — arm the debounce — so the pass
    /// reports `worktree`, not a rescan to run now. Two in one pass are one flag: one
    /// deadline, re-armed from the last of them.
    #[test]
    fn run_drain_reports_a_worktree_event_for_the_debounce_not_for_a_rescan() {
        use lastcall_engine::herdr::client::WorktreeChange;
        let worktree = |change| HerdrEvent::WorktreeChanged {
            change,
            workspace_id: "ws".to_owned(),
            path: "/tmp/w/alpha-feat".to_owned(),
            branch: None,
        };
        let mut ui = ui();
        let mut wires = Wires::new();
        let (htx, hrx) = mpsc::channel(8);
        wires.link.events = Some(hrx);
        htx.try_send(worktree(WorktreeChange::Created)).unwrap();
        htx.try_send(worktree(WorktreeChange::Removed)).unwrap();
        let pass = wires.drain_into(&mut ui, (Changed::No, None));
        assert!(pass.worktree, "the drained events ask for the debounce");
        assert_eq!(
            pass.changed,
            Changed::No,
            "a worktree event draws nothing by itself"
        );
        assert!(pass.effects.is_empty() && pass.stop.is_none());

        // A pass with no worktree event leaves the flag down.
        let pass = wires.drain_into(&mut ui, (Changed::No, None));
        assert!(!pass.worktree);
    }

    /// The cap bounds one pass: a queue longer than [`DRAIN_CAP`] draws, then continues.
    #[test]
    fn run_drain_stops_at_the_cap() {
        let mut ui = ui();
        let mut wires = Wires::new();
        for _ in 0..DRAIN_CAP + 8 {
            wires.input_tx.send(key(KeyCode::Char('f'))).unwrap();
        }
        let pass = wires.drain_into(&mut ui, (Changed::No, None));
        assert_eq!(pass.changed, Changed::Yes);
        // Eight are still queued for the next pass.
        let mut left = 0;
        while wires.input_rx.try_recv().is_ok() {
            left += 1;
        }
        assert_eq!(left, 8);
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
