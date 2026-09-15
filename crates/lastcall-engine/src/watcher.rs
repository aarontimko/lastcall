//! The watcher loop (kickoff deliverable 11): one `notify` watcher over every root, git-dir
//! events filtered to an allowlist, trailing-edge debounce, and polling backstops for
//! HEAD (`head_poll`) and root discovery (`rescan`). Filesystem events only *schedule*
//! work; every scan, head inspection and rescan runs on `spawn_blocking` under the
//! engine's mutex, and the outcome is published as an [`EngineEvent`].
//!
//! `ignore_globs` scope the watcher only: an ignored path never wakes a scan, but the next
//! scan (manual, or the `rescan` backstop) still shows the tracked edit.
//!
//! `Access` events (opens, reads, read-only closes) never schedule work: on Linux they are
//! the scan's own reads coming back through inotify ([`actionable`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use notify::{RecursiveMode, Watcher as _};
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::engine::{Engine, HeadChange};
use crate::git::Oid;
use crate::roots::RootsChanged;
use crate::scan::Pile;

/// Injectable timings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineTimings {
    /// Trailing-edge debounce for worktree events before a root scan.
    pub debounce: Duration,
    /// Starvation cap on that trailing edge: a root's scan is never postponed past this
    /// long after the **first** event of a burst, so a writer that never pauses for
    /// `debounce` still gets scanned (Amendment v1.6; §10 2026-09-05 ruling 2).
    /// Hardcoded like `debounce`, not a config key (Q6).
    pub debounce_max: Duration,
    /// Backstop: re-inspect every git root's HEAD this often.
    pub head_poll: Duration,
    /// Backstop: re-run discovery and scan every root this often.
    pub rescan: Duration,
}

impl Default for EngineTimings {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(750),
            debounce_max: Duration::from_secs(3),
            head_poll: Duration::from_secs(10),
            rescan: Duration::from_secs(30),
        }
    }
}

/// Schedule a root's scan on the trailing edge, under the starvation cap: due `debounce`
/// after this event, but never later than `debounce_max` after the first event of the
/// burst. `first_seen` remembers where the burst started; [`scanned`] ends it.
///
/// Without the cap the deadline slides forever under a writer that never pauses 750 ms —
/// an app appending to a log inside the repo — and that root's *other* files never reach
/// the reviewer either, because the scan is per root (§11, 2026-09-05).
fn schedule(
    due: &mut BTreeMap<PathBuf, Instant>,
    first_seen: &mut BTreeMap<PathBuf, Instant>,
    root: PathBuf,
    now: Instant,
    timings: &EngineTimings,
) {
    let first = *first_seen.entry(root.clone()).or_insert(now);
    due.insert(
        root,
        (now + timings.debounce).min(first + timings.debounce_max),
    );
}

/// A root was scanned: the burst is over as far as the cap is concerned, so the next event
/// opens a fresh `debounce_max` window. Paired with [`schedule`] and called from **every**
/// path that scans a root — the `ready` drain (which is also where the rescan and
/// watcher-error paths land, since both schedule `now`) and `inspect_root`'s HEAD-change
/// scan. Miss one and the cap re-fires immediately on the next event of a long burst.
fn scanned(first_seen: &mut BTreeMap<PathBuf, Instant>, root: &Path) {
    first_seen.remove(root);
}

/// Deliverable 8's watcher probes. The loop's decisions are invisible from the outside —
/// an event that scheduled nothing and an event that never arrived look identical on
/// screen — so each one gets a `debug` line with stable field names.
///
/// `reason` is a closed vocabulary: `event` (a filesystem event under the root), `head`
/// (HEAD moved, so `inspect_head` scanned), `rescan` (the discovery backstop, a watcher
/// error, or a root set that changed) and `refresh` (the initial scans and the catch-up
/// after watches install).
fn trace_scan_due(root: &Path, reason: &'static str) {
    tracing::debug!(root = %root.display(), reason, "scan due");
}

/// What the watcher publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    /// A root was scanned. `seq` is the engine-global number of that scan
    /// ([`Engine::scan_seq`]): a consumer holding a newer pile for `root` drops an older one.
    Pile {
        root: PathBuf,
        seq: u64,
        pile: Pile,
    },
    /// A root's HEAD moved (or an in-progress operation finished).
    Head {
        root: PathBuf,
        from: Option<Oid>,
        to: Option<Oid>,
        branch: Option<String>,
        notice: Option<String>,
    },
    RootsChanged(RootsChanged),
    Notice {
        root: Option<PathBuf>,
        text: String,
    },
    /// Progress only: `root`'s scan just returned on the pool with `rows` pending rows; its
    /// pile follows when the whole `scan_all` lands (piles arrive together, in path order).
    /// Sent with `try_send` from under the engine lock, so a full channel drops one rather
    /// than parking a worker — a consumer counts these, never waits on them.
    Scanned {
        root: PathBuf,
        rows: usize,
    },
}

/// The running watcher: the event stream, the shared engine, and a stop switch.
pub struct Watcher {
    pub events: mpsc::Receiver<EngineEvent>,
    pub engine: Arc<Mutex<Engine>>,
    pub handle: JoinHandle<()>,
    stop: watch::Sender<bool>,
    rescan: Arc<Notify>,
}

impl Watcher {
    /// Ask the loop to finish; `handle` completes shortly after.
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }

    /// Run the discovery rescan now instead of at the next backstop tick.
    ///
    /// Exactly what the `rescan` interval does: `Engine::rescan`, then — if the root set
    /// changed — reinstalled watches, an `EngineEvent::RootsChanged`, and every root marked
    /// due for a scan. `Engine::rescan` opens new roots but does not scan or watch them, so
    /// this loop is the only thing that turns a new directory into a visible root, and the
    /// backstop is minutes wide.
    ///
    /// The caller is whoever learns about a new root sooner than the backstop would: herdr's
    /// `WorktreeChanged`, after its own debounce (Phase 5 deliverable 7). This is a
    /// *trigger*, not an instruction — `roots::discover` still decides, so a `Removed` path
    /// that is still on disk stays a root. Calls coalesce: several before the loop wakes run
    /// one rescan, and one arriving mid-rescan runs another after it.
    pub fn request_rescan(&self) {
        self.rescan.notify_one();
    }

    /// The same trigger as a value, so a caller that no longer has the watcher in hand can
    /// still ask for the rescan (deliverable 8).
    ///
    /// The TUI never takes the engine lock on the task that draws: it hands the work to a
    /// blocking thread and comes back. That thread cannot borrow the watcher, so the depth
    /// the tour applies is set under the lock over there and the rescan asked for here,
    /// after the guard is gone. Cloning it is free and the clones coalesce exactly as
    /// [`Watcher::request_rescan`] does.
    pub fn rescan_trigger(&self) -> RescanTrigger {
        RescanTrigger(Arc::clone(&self.rescan))
    }

    pub async fn join(self) {
        let _ = self.stop.send(true);
        let _ = self.handle.await;
    }
}

/// A clonable handle on the watcher's rescan trigger. Holding one keeps nothing alive that
/// matters: a notify with no loop listening is a no-op.
#[derive(Debug, Clone)]
pub struct RescanTrigger(Arc<Notify>);

impl RescanTrigger {
    /// [`Watcher::request_rescan`], from wherever the handle got to.
    pub fn request_rescan(&self) {
        self.0.notify_one();
    }
}

/// Lock the shared engine, tolerating a poisoned mutex (a panicked scan must not take the
/// watcher down with it).
pub fn lock(engine: &Arc<Mutex<Engine>>) -> MutexGuard<'_, Engine> {
    engine.lock().unwrap_or_else(|p| p.into_inner())
}

impl Engine {
    /// Start the watcher loop on the current tokio runtime.
    pub fn run(self, timings: EngineTimings) -> Watcher {
        let engine = Arc::new(Mutex::new(self));
        let (tx, events) = mpsc::channel(256);
        let (stop, stop_rx) = watch::channel(false);
        let rescan = Arc::new(Notify::new());
        let handle = tokio::spawn(run_loop(
            engine.clone(),
            timings,
            tx,
            stop_rx,
            rescan.clone(),
        ));
        Watcher {
            events,
            engine,
            handle,
            stop,
            rescan,
        }
    }
}

/// Per-root watch facts.
#[derive(Debug, Clone)]
struct RootWatch {
    path: PathBuf,
    /// The parent dir the root was discovered under; the unit of watching.
    parent: PathBuf,
    git_dir: Option<PathBuf>,
    common_dir: Option<PathBuf>,
}

/// Git-dir paths whose changes mean "HEAD may have moved".
pub fn git_dir_allowlisted(rel: &Path) -> bool {
    let mut comps = rel.components();
    let Some(first) = comps.next() else {
        return false;
    };
    let first = first.as_os_str().to_string_lossy();
    let single = comps.next().is_none();
    match first.as_ref() {
        "HEAD" | "index" | "ORIG_HEAD" | "MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD"
        | "packed-refs" => single,
        "refs" | "rebase-merge" | "rebase-apply" | "logs" => true,
        _ => false,
    }
}

/// Whether a filesystem event can mean the tree changed. `notify`'s inotify backend also
/// subscribes to `IN_OPEN` and `IN_CLOSE_NOWRITE`, so on Linux every file and directory git
/// opens *during a scan* comes back as an `Access` event under the root; scheduling a scan
/// on those made each scan trigger the next one. A close-after-write is the one access
/// that signals a change (the write itself already arrived as `Modify`).
pub fn actionable(kind: &notify::EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode, EventKind};
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false,
        _ => true,
    }
}

enum Scheduled {
    Scan(PathBuf),
    Head(PathBuf),
    Ignore,
}

impl Scheduled {
    /// The `scheduled=` field of the `watch event` probe.
    fn label(&self) -> &'static str {
        match self {
            Scheduled::Scan(_) => "scan",
            Scheduled::Head(_) => "head",
            Scheduled::Ignore => "ignore",
        }
    }
}

fn classify_path(path: &Path, roots: &[RootWatch], ignore: &globset::GlobSet) -> Scheduled {
    // Git-dir hits first: a linked worktree's git dir lives outside its root, *inside* the
    // main worktree's (`<main>/.git/worktrees/<name>`), so the longest matching dir wins
    // and, at equal length, a root's own `git_dir` beats another's shared `common_dir`.
    let mut best: Option<(&RootWatch, &Path, bool)> = None;
    for r in roots {
        for (dir, own) in [(&r.git_dir, true), (&r.common_dir, false)] {
            let Some(dir) = dir else { continue };
            if !path.starts_with(dir) {
                continue;
            }
            let better = match best {
                None => true,
                Some((_, b, b_own)) => {
                    let (l, bl) = (dir.as_os_str().len(), b.as_os_str().len());
                    l > bl || (l == bl && own && !b_own)
                }
            };
            if better {
                best = Some((r, dir.as_path(), own));
            }
        }
    }
    if let Some((r, dir, _)) = best {
        let rel = path.strip_prefix(dir).unwrap_or(path);
        return if git_dir_allowlisted(rel) {
            Scheduled::Head(r.path.clone())
        } else {
            Scheduled::Ignore
        };
    }
    // Longest root prefix wins so nested repos own their files.
    let best = roots
        .iter()
        .filter(|r| path.starts_with(&r.path))
        .max_by_key(|r| r.path.as_os_str().len());
    let Some(root) = best else {
        return Scheduled::Ignore;
    };
    let rel = path.strip_prefix(&root.path).unwrap_or(path);
    if rel
        .components()
        .next()
        .is_some_and(|c| c.as_os_str() == ".git")
    {
        return Scheduled::Ignore;
    }
    if ignore.is_match(rel) {
        return Scheduled::Ignore;
    }
    Scheduled::Scan(root.path.clone())
}

fn root_watches(engine: &Engine) -> Vec<RootWatch> {
    engine
        .roots()
        .into_iter()
        .map(|r| RootWatch {
            path: r.path.clone(),
            parent: r.parent.clone(),
            git_dir: r.repo.as_ref().map(|_| r.head.git_dir.clone()),
            common_dir: r
                .repo
                .as_ref()
                .filter(|_| r.head.common_dir != r.head.git_dir)
                .map(|_| r.head.common_dir.clone()),
        })
        .collect()
}

/// The recursive watches a set of roots needs: each root's **parent dir** (one watch covers
/// every root under it — on macOS each `watch` call re-registers the FSEvents stream, which
/// can take seconds), plus any root not under one of those, plus every git dir that lives
/// outside them (a linked worktree's common dir, D10).
fn wanted_watches(roots: &[RootWatch]) -> BTreeSet<PathBuf> {
    let mut wanted: BTreeSet<PathBuf> = roots.iter().map(|r| r.parent.clone()).collect();
    let covered = |p: &Path, w: &BTreeSet<PathBuf>| w.iter().any(|d| p.starts_with(d));
    for r in roots {
        if !covered(&r.path, &wanted) {
            wanted.insert(r.path.clone());
        }
    }
    for r in roots {
        for dir in [&r.git_dir, &r.common_dir].into_iter().flatten() {
            if !covered(dir, &wanted) {
                wanted.insert(dir.clone());
            }
        }
    }
    wanted
}

/// What a finished [`spawn_install`] hands back.
type Installed = (
    Option<notify::RecommendedWatcher>,
    BTreeSet<PathBuf>,
    Vec<String>,
);

/// Run [`install_watches`] off the runtime: `notify`'s `watch` blocks until the platform
/// stream is registered, seconds on some macOS hosts, and the initial scans must not wait
/// for it. The watcher and the watched set travel with the task and come back with it.
fn spawn_install(
    watcher: Option<notify::RecommendedWatcher>,
    watched: BTreeSet<PathBuf>,
    roots: Vec<RootWatch>,
) -> JoinHandle<Installed> {
    tokio::task::spawn_blocking(move || {
        let mut watcher = watcher;
        let mut watched = watched;
        let notices = install_watches(&mut watcher, &mut watched, &roots);
        (watcher, watched, notices)
    })
}

/// (Re)install the recursive watches of [`wanted_watches`]. Blocking; see [`spawn_install`].
fn install_watches(
    watcher: &mut Option<notify::RecommendedWatcher>,
    watched: &mut BTreeSet<PathBuf>,
    roots: &[RootWatch],
) -> Vec<String> {
    let mut notices = Vec::new();
    let Some(w) = watcher.as_mut() else {
        return notices;
    };
    let wanted = wanted_watches(roots);
    for gone in watched.difference(&wanted).cloned().collect::<Vec<_>>() {
        let _ = w.unwatch(&gone);
        watched.remove(&gone);
    }
    for p in wanted.difference(watched).cloned().collect::<Vec<_>>() {
        match w.watch(&p, RecursiveMode::Recursive) {
            Ok(()) => {
                watched.insert(p);
            }
            Err(e) => notices.push(format!("cannot watch {}: {e}", p.display())),
        }
    }
    notices
}

/// Run one engine call on `spawn_blocking` under the mutex and hand back its result: the
/// only way a task on the runtime should touch the engine (a guard held across an
/// `.await` stalls every scan). The UI runs every engine call through this.
pub async fn blocking<T: Send + 'static>(
    engine: &Arc<Mutex<Engine>>,
    f: impl FnOnce(&mut Engine) -> T + Send + 'static,
) -> T {
    let engine = engine.clone();
    tokio::task::spawn_blocking(move || {
        let mut guard = lock(&engine);
        f(&mut guard)
    })
    .await
    .expect("engine work does not panic")
}

async fn emit(tx: &mpsc::Sender<EngineEvent>, event: EngineEvent) -> bool {
    tx.send(event).await.is_ok()
}

async fn scan_root(
    engine: &Arc<Mutex<Engine>>,
    tx: &mpsc::Sender<EngineEvent>,
    root: PathBuf,
) -> bool {
    let r = root.clone();
    // The seq is read under the same lock as the scan it numbers.
    match blocking(engine, move |e| e.scan(&r).map(|pile| (e.scan_seq(), pile))).await {
        Ok((seq, pile)) => emit(tx, EngineEvent::Pile { root, seq, pile }).await,
        Err(e) => {
            emit(
                tx,
                EngineEvent::Notice {
                    root: Some(root),
                    text: format!("scan failed: {e}"),
                },
            )
            .await
        }
    }
}

/// Scan every root in one engine call — the bounded pool — and emit the piles in path
/// order. One lock for the whole set instead of one per root.
async fn scan_all_roots(engine: &Arc<Mutex<Engine>>, tx: &mpsc::Sender<EngineEvent>) -> bool {
    let progress = tx.clone();
    let results = blocking(engine, move |e| {
        e.scan_all_with(&|root, rows| {
            // From the pool thread, under the engine lock: never block here — the consumer
            // may be the one waiting for the lock. A dropped tick costs one ✓ until the
            // pile lands.
            let _ = progress.try_send(EngineEvent::Scanned {
                root: root.to_path_buf(),
                rows,
            });
        })
    })
    .await;
    for (root, seq, result) in results {
        let ok = match result {
            Ok(pile) => emit(tx, EngineEvent::Pile { root, seq, pile }).await,
            Err(e) => {
                emit(
                    tx,
                    EngineEvent::Notice {
                        root: Some(root),
                        text: format!("scan failed: {e}"),
                    },
                )
                .await
            }
        };
        if !ok {
            return false;
        }
    }
    true
}

/// The outcome of one head inspection: whether the consumer is still listening, and
/// whether the inspection **scanned** the root (`inspect_head` scans only when HEAD moved).
/// The second half is what clears the root's starvation-cap window ([`scanned`]).
struct Inspected {
    alive: bool,
    scanned: bool,
}

async fn inspect_root(
    engine: &Arc<Mutex<Engine>>,
    tx: &mpsc::Sender<EngineEvent>,
    root: PathBuf,
) -> Inspected {
    let r = root.clone();
    let inspected = blocking(engine, move |e| e.inspect_head(&r)).await;
    tracing::debug!(
        root = %root.display(),
        changed = matches!(inspected, Ok(Some(_))),
        "head inspect"
    );
    match inspected {
        Ok(Some(HeadChange {
            root,
            from,
            to,
            branch,
            notice,
            seq,
            pile,
        })) => {
            trace_scan_due(&root, "head");
            let alive = emit(
                tx,
                EngineEvent::Head {
                    root: root.clone(),
                    from,
                    to,
                    branch,
                    notice,
                },
            )
            .await
                && emit(tx, EngineEvent::Pile { root, seq, pile }).await;
            Inspected {
                alive,
                scanned: true,
            }
        }
        Ok(None) => Inspected {
            alive: true,
            scanned: false,
        },
        Err(e) => {
            let alive = emit(
                tx,
                EngineEvent::Notice {
                    root: Some(root),
                    text: format!("head inspection failed: {e}"),
                },
            )
            .await;
            Inspected {
                alive,
                scanned: false,
            }
        }
    }
}

async fn run_loop(
    engine: Arc<Mutex<Engine>>,
    timings: EngineTimings,
    tx: mpsc::Sender<EngineEvent>,
    mut stop: watch::Receiver<bool>,
    // `Watcher::request_rescan`: the discovery rescan on demand, not only on the tick.
    requested_rescan: Arc<Notify>,
) {
    let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<Result<notify::Event, notify::Error>>();
    let mut watcher = match notify::recommended_watcher(move |res| {
        let _ = fs_tx.send(res);
    }) {
        Ok(w) => Some(w),
        Err(e) => {
            if !emit(
                &tx,
                EngineEvent::Notice {
                    root: None,
                    text: format!("filesystem watcher unavailable, polling only: {e}"),
                },
            )
            .await
            {
                return;
            }
            None
        }
    };
    let mut watched: BTreeSet<PathBuf> = BTreeSet::new();
    let (mut roots, ignore) = {
        let g = lock(&engine);
        (root_watches(&g), g.ignore_globs().clone())
    };
    // Watches install off the runtime while the initial scans run; when they land, every
    // root is scanned and inspected once more so nothing from the gap is missed.
    let mut install: Option<JoinHandle<Installed>> = Some(spawn_install(
        watcher.take(),
        std::mem::take(&mut watched),
        roots.clone(),
    ));
    let mut reinstall = false;
    // Initial scans: every root through one `scan_all` on the engine's bounded pool (Phase
    // 5 deliverable 1b), each root reported as `Scanned` the moment it finishes, the piles
    // landing together in path order. The first root used to be scanned on its own so its
    // pile reached the screen early; the Gate 8 sponsor run ruled that frame misleading
    // (one repo listed as if it were the only one with changes while the rest were still
    // being scanned), so the TUI lists nothing until every root has reported and the head
    // start bought nothing — `first_pile_ms` on the bench is now the batch's arrival.
    if !scan_all_roots(&engine, &tx).await {
        return;
    }

    let mut due: BTreeMap<PathBuf, Instant> = BTreeMap::new();
    // When each root's current burst of worktree events began; the starvation cap's input.
    //
    // It is **loop-local on purpose**, which means a scan the loop did not initiate does
    // not end the window (verifier (a) F7): the TUI's own `watcher::blocking(|e|
    // e.scan(root))` — `Effect::Refresh`, and the rescan an accept leaves behind — is
    // invisible here, so a burst window opened before such a scan still fires its cap up
    // to `debounce_max` later. The cost is bounded at one redundant scan per window, whose
    // pile is unchanged and so is `Changed::No` at the app, and the window then resets.
    // Accepted, same class and same bound as `inspect_root` leaving `due` in place on a
    // HeadChange; the alternative is sharing this map across a task boundary for a scan
    // that has already happened.
    let mut first_seen: BTreeMap<PathBuf, Instant> = BTreeMap::new();
    let mut head_due: BTreeSet<PathBuf> = BTreeSet::new();
    let mut head_poll = tokio::time::interval(timings.head_poll);
    head_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    head_poll.tick().await; // the immediate first tick
    let mut rescan = tokio::time::interval(timings.rescan);
    rescan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    rescan.tick().await;

    let mut rescan_now = false;
    loop {
        let next_due = due.values().min().copied();
        let sleep = tokio::time::sleep_until(
            next_due.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600)),
        );
        tokio::select! {
            _ = stop.changed() => break,
            done = async { install.as_mut().expect("guarded by the branch condition").await }, if install.is_some() => {
                install = None;
                match done {
                    Ok((w, set, notices)) => {
                        watcher = w;
                        watched = set;
                        for n in notices {
                            if !emit(&tx, EngineEvent::Notice { root: None, text: n }).await { return; }
                        }
                        if reinstall {
                            reinstall = false;
                            install = Some(spawn_install(watcher.take(), std::mem::take(&mut watched), roots.clone()));
                        } else {
                            // Close the gap between the initial scans and the live watch,
                            // then say the watch is live; the rescan backstop counts from
                            // here.
                            for r in roots.clone() {
                                let ok = if r.git_dir.is_some() {
                                    let done = inspect_root(&engine, &tx, r.path.clone()).await;
                                    if done.scanned { scanned(&mut first_seen, &r.path); }
                                    done.alive
                                } else {
                                    scanned(&mut first_seen, &r.path);
                                    trace_scan_due(&r.path, "refresh");
                                    scan_root(&engine, &tx, r.path).await
                                };
                                if !ok { return; }
                            }
                            rescan.reset();
                            tracing::debug!(roots = roots.len(), "watch installed");
                            let text = format!(
                                "watching {} ({} root{})",
                                watched.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
                                roots.len(),
                                if roots.len() == 1 { "" } else { "s" }
                            );
                            if !emit(&tx, EngineEvent::Notice { root: None, text }).await { return; }
                        }
                    }
                    Err(e) => {
                        if !emit(&tx, EngineEvent::Notice { root: None, text: format!("watch installation failed, polling only: {e}") }).await { return; }
                    }
                }
            }
            res = fs_rx.recv() => {
                let Some(res) = res else { break };
                match res {
                    Err(e) => {
                        if !emit(&tx, EngineEvent::Notice { root: None, text: format!("watcher error, rescanning: {e}") }).await { return; }
                        let now = Instant::now();
                        for r in &roots { trace_scan_due(&r.path, "rescan"); due.insert(r.path.clone(), now); }
                    }
                    Ok(ev) => {
                        if ev.need_rescan() {
                            let now = Instant::now();
                            for r in &roots { trace_scan_due(&r.path, "rescan"); due.insert(r.path.clone(), now); }
                        }
                        if actionable(&ev.kind) {
                            for p in &ev.paths {
                                let scheduled = classify_path(p, &roots, &ignore);
                                tracing::debug!(
                                    path = %p.display(),
                                    kind = ?ev.kind,
                                    scheduled = scheduled.label(),
                                    "watch event"
                                );
                                match scheduled {
                                    Scheduled::Scan(root) => {
                                        trace_scan_due(&root, "event");
                                        schedule(&mut due, &mut first_seen, root, Instant::now(), &timings);
                                    }
                                    Scheduled::Head(root) => { head_due.insert(root); }
                                    Scheduled::Ignore => {}
                                }
                            }
                        }
                    }
                }
            }
            _ = head_poll.tick() => {
                for r in &roots {
                    if r.git_dir.is_some() { head_due.insert(r.path.clone()); }
                }
            }
            _ = rescan.tick() => { rescan_now = true; }
            // The same work, asked for rather than waited for (deliverable 7's seam).
            _ = requested_rescan.notified() => { rescan_now = true; }
            _ = sleep, if next_due.is_some() => {}
        }

        if std::mem::take(&mut rescan_now) {
            tracing::debug!("rescan backstop");
            let changed = blocking(&engine, |e| e.rescan()).await;
            match changed {
                Ok(changed) => {
                    if !changed.is_empty() {
                        roots = {
                            let g = lock(&engine);
                            root_watches(&g)
                        };
                        if install.is_some() {
                            reinstall = true;
                        } else {
                            install = Some(spawn_install(
                                watcher.take(),
                                std::mem::take(&mut watched),
                                roots.clone(),
                            ));
                        }
                        if !emit(&tx, EngineEvent::RootsChanged(changed)).await {
                            return;
                        }
                    }
                }
                Err(e) => {
                    if !emit(
                        &tx,
                        EngineEvent::Notice {
                            root: None,
                            text: format!("rescan failed: {e}"),
                        },
                    )
                    .await
                    {
                        return;
                    }
                }
            }
            let now = Instant::now();
            for r in &roots {
                trace_scan_due(&r.path, "rescan");
                due.insert(r.path.clone(), now);
            }
        }

        // Drain: head inspections first (they scan too), then due scans. Both end the
        // root's starvation-cap window, so a burst that outlives a scan starts counting
        // its 3 s again from the next event instead of firing the cap right away.
        for root in std::mem::take(&mut head_due) {
            let done = inspect_root(&engine, &tx, root.clone()).await;
            if done.scanned {
                scanned(&mut first_seen, &root);
            }
            if !done.alive {
                return;
            }
        }
        let now = Instant::now();
        let ready: Vec<PathBuf> = due
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(p, _)| p.clone())
            .collect();
        for root in ready {
            due.remove(&root);
            scanned(&mut first_seen, &root);
            if !scan_root(&engine, &tx, root).await {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replays one root's burst against [`schedule`]/[`scanned`] on a simulated clock and
    /// returns the millisecond offsets at which the loop would have scanned it.
    ///
    /// The model is the loop's own: the `select!` wakes at whichever comes first, the
    /// event or `sleep_until(next_due)`, and the drain at the end of the pass scans every
    /// root whose deadline has passed (the `head_change_at` hook stands in for a
    /// HEAD-change scan arriving between events).
    ///
    /// **Ties are the simulator's, not the loop's** (verifier (a) F6): the real
    /// `tokio::select!` above has no `biased;`, so when an event lands exactly on a
    /// deadline either arm may win, and the two orders differ by nothing that matters —
    /// timer first scans in this pass and the event opens the next window, so a
    /// `Changed::No` scan may follow 750 ms later; event first schedules a deadline that
    /// is already past and the same pass's drain scans it, carrying the tie event's own
    /// content. This model resolves the tie to the timer so the expected offsets below are
    /// a single sequence; `biased;` is deliberately *not* added to the loop for it.
    fn replay(events_ms: &[u64], head_change_at: Option<u64>, timings: &EngineTimings) -> Vec<u64> {
        let t0 = Instant::now();
        let root = PathBuf::from("/w/alpha");
        let mut due: BTreeMap<PathBuf, Instant> = BTreeMap::new();
        let mut first_seen: BTreeMap<PathBuf, Instant> = BTreeMap::new();
        let mut scans: Vec<u64> = Vec::new();
        let ms = |at: Instant| (at - t0).as_millis() as u64;

        let mut pending: Vec<u64> = events_ms.to_vec();
        pending.sort_unstable();
        let mut next = 0usize;
        loop {
            let next_event = pending.get(next).copied();
            let next_due = due.get(&root).map(|d| ms(*d));
            // The head-change scan is its own wake-up, ordered with everything else.
            let head = head_change_at.filter(|at| scans.iter().all(|s| s != at));
            let Some(now) = [next_due, head, next_event].into_iter().flatten().min() else {
                break;
            };
            if next_due == Some(now) {
                // The drain: the deadline passed, so the root is scanned.
                due.remove(&root);
                scanned(&mut first_seen, &root);
                scans.push(now);
                continue;
            }
            if head == Some(now) {
                // `inspect_head` scanned this root; only the cap window is cleared.
                scanned(&mut first_seen, &root);
                scans.push(now);
                continue;
            }
            next += 1;
            schedule(
                &mut due,
                &mut first_seen,
                root.clone(),
                t0 + Duration::from_millis(now),
                timings,
            );
        }
        scans
    }

    /// B12 (Amendment v1.6): a writer that never pauses for the 750 ms trailing edge is
    /// still scanned about every 3 s, and the burst's last state lands one trailing edge
    /// after the writer stops. Without the cap the only scan in six seconds would be the
    /// one 750 ms after the end — and on a real root the 30 s rescan backstop would be
    /// the first thing to break the starvation.
    #[test]
    fn watcher_debounce_cap_scans_a_never_quiet_root_about_every_three_seconds() {
        let timings = EngineTimings::default();
        assert_eq!(timings.debounce, Duration::from_millis(750));
        assert_eq!(timings.debounce_max, Duration::from_secs(3));
        // Six seconds of edits, 300 ms apart: never 750 ms of quiet until the end.
        let events: Vec<u64> = (0..=20).map(|i| i * 300).collect();
        assert_eq!(*events.last().unwrap(), 6_000);

        let scans = replay(&events, None, &timings);
        assert_eq!(
            scans,
            vec![3_000, 6_000, 6_750],
            "a scan at the cap, another a cap later, then the trailing edge after the end"
        );

        // Without the cap, the same burst produces exactly one scan, at the very end —
        // the sponsor's "no changes until the very end" (§10 2026-09-04).
        let uncapped = EngineTimings {
            debounce_max: Duration::from_secs(60 * 60),
            ..timings
        };
        assert_eq!(replay(&events, None, &uncapped), vec![6_750]);
    }

    /// A HEAD-change scan mid-burst ends the burst's cap window too, so the cap does not
    /// fire again moments after the root was already scanned: the next capped scan is one
    /// full `debounce_max` after the first event that follows the head change.
    #[test]
    fn watcher_a_head_change_scan_mid_burst_leaves_no_redundant_capped_scan() {
        let timings = EngineTimings::default();
        let events: Vec<u64> = (0..=20).map(|i| i * 300).collect();

        // A commit lands at 1,000 ms; `inspect_head` scans the root.
        let scans = replay(&events, Some(1_000), &timings);
        assert_eq!(scans.first(), Some(&1_000), "the head-change scan");
        assert_eq!(
            scans.get(1),
            Some(&4_200),
            "the next cap is 3 s after the first event *after* the head change (1,200), \
             not the 3,000 the original window would have fired"
        );
        assert!(
            !scans.contains(&3_000),
            "no redundant capped scan left behind: {scans:?}"
        );

        // The control: without the head change the cap fires at 3,000.
        assert_eq!(replay(&events, None, &timings).first(), Some(&3_000));
    }

    /// The cap never *delays* a scan: a quiet writer still gets the plain trailing edge.
    #[test]
    fn watcher_debounce_cap_does_not_move_a_quiet_root() {
        let timings = EngineTimings::default();
        // Two edits two seconds apart: each is its own burst.
        assert_eq!(replay(&[0, 2_000], None, &timings), vec![750, 2_750]);
    }

    #[test]
    fn watcher_wanted_watches_are_parent_dirs_plus_external_git_dirs() {
        let rw =
            |path: &str, parent: &str, git_dir: Option<&str>, common: Option<&str>| RootWatch {
                path: PathBuf::from(path),
                parent: PathBuf::from(parent),
                git_dir: git_dir.map(PathBuf::from),
                common_dir: common.map(PathBuf::from),
            };
        let roots = vec![
            rw("/w/alpha", "/w", Some("/w/alpha/.git"), None),
            rw("/w/beta", "/w", Some("/w/beta/.git"), None),
            rw("/w/notes", "/w", None, None),
            // A linked worktree whose common dir lives outside every parent dir.
            rw(
                "/w/wt",
                "/w",
                Some("/elsewhere/main/.git/worktrees/wt"),
                Some("/elsewhere/main/.git"),
            ),
            // An ad-hoc root that is its own parent.
            rw("/adhoc", "/adhoc", Some("/adhoc/.git"), None),
        ];
        let wanted: Vec<PathBuf> = wanted_watches(&roots).into_iter().collect();
        assert_eq!(
            wanted,
            vec![
                PathBuf::from("/adhoc"),
                PathBuf::from("/elsewhere/main/.git"),
                PathBuf::from("/elsewhere/main/.git/worktrees/wt"),
                PathBuf::from("/w"),
            ]
        );
    }

    #[test]
    fn watcher_access_events_never_schedule_work() {
        use notify::event::{
            AccessKind, AccessMode, CreateKind, DataChange, EventKind, ModifyKind,
        };
        // What inotify reports for the scan's own reads.
        assert!(!actionable(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!actionable(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(!actionable(&EventKind::Access(AccessKind::Read)));
        assert!(!actionable(&EventKind::Access(AccessKind::Any)));
        // What a change looks like.
        assert!(actionable(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(actionable(&EventKind::Modify(ModifyKind::Data(
            DataChange::Any
        ))));
        assert!(actionable(&EventKind::Create(CreateKind::File)));
        assert!(actionable(&EventKind::Remove(
            notify::event::RemoveKind::Any
        )));
        assert!(actionable(&EventKind::Any));
    }

    #[test]
    fn watcher_git_dir_allowlist() {
        for ok in [
            "HEAD",
            "index",
            "ORIG_HEAD",
            "MERGE_HEAD",
            "packed-refs",
            "refs/heads/main",
            "refs/remotes/origin/main",
            "rebase-merge/done",
            "rebase-apply/next",
            "logs/HEAD",
        ] {
            assert!(git_dir_allowlisted(Path::new(ok)), "{ok}");
        }
        for no in [
            "objects/ab/cdef",
            "config",
            "index.lock",
            "HEAD/x",
            "hooks/pre-commit",
            "",
        ] {
            assert!(!git_dir_allowlisted(Path::new(no)), "{no}");
        }
    }

    #[test]
    fn watcher_classify_routes_git_dir_worktree_and_ignored_paths() {
        let roots = vec![
            RootWatch {
                path: PathBuf::from("/w/a"),
                parent: PathBuf::from("/w"),
                git_dir: Some(PathBuf::from("/w/a/.git")),
                common_dir: None,
            },
            RootWatch {
                path: PathBuf::from("/w/a/inner"),
                parent: PathBuf::from("/w/a"),
                git_dir: Some(PathBuf::from("/w/a/inner/.git")),
                common_dir: None,
            },
            RootWatch {
                path: PathBuf::from("/w/wt"),
                parent: PathBuf::from("/w"),
                git_dir: Some(PathBuf::from("/w/main/.git/worktrees/wt")),
                common_dir: Some(PathBuf::from("/w/main/.git")),
            },
        ];
        let ignore = crate::engine::build_globs(&["vendor/**".to_owned()]);
        let is = |p: &str| classify_path(Path::new(p), &roots, &ignore);
        assert!(matches!(is("/w/a/src/x.rs"), Scheduled::Scan(r) if r == Path::new("/w/a")));
        assert!(matches!(is("/w/a/inner/y"), Scheduled::Scan(r) if r == Path::new("/w/a/inner")));
        assert!(matches!(is("/w/a/.git/HEAD"), Scheduled::Head(r) if r == Path::new("/w/a")));
        assert!(matches!(is("/w/a/.git/objects/ab/cd"), Scheduled::Ignore));
        assert!(matches!(is("/w/a/vendor/x"), Scheduled::Ignore));
        assert!(
            matches!(is("/w/main/.git/refs/heads/main"), Scheduled::Head(r) if r == Path::new("/w/wt"))
        );
        assert!(
            matches!(is("/w/main/.git/worktrees/wt/HEAD"), Scheduled::Head(r) if r == Path::new("/w/wt"))
        );
        assert!(matches!(is("/elsewhere/z"), Scheduled::Ignore));

        // Deliverable 8: the `scheduled=` field of the `watch event` probe is exactly this
        // three-word vocabulary, so a log can be grepped for the events that scheduled
        // nothing — the shape of "my edit never reached the screen".
        assert_eq!(is("/w/a/src/x.rs").label(), "scan");
        assert_eq!(is("/w/a/.git/HEAD").label(), "head");
        assert_eq!(is("/w/a/vendor/x").label(), "ignore");
    }

    /// A main worktree and its linked worktree are both roots: the linked worktree's git
    /// dir is a subdirectory of main's, so its `HEAD`/`index` events must route to it,
    /// not fail main's allowlist as `worktrees/wt/HEAD`.
    #[test]
    fn watcher_classify_routes_linked_worktree_git_dir_events_to_the_worktree() {
        let roots = vec![
            RootWatch {
                path: PathBuf::from("/w/main"),
                parent: PathBuf::from("/w"),
                git_dir: Some(PathBuf::from("/w/main/.git")),
                common_dir: Some(PathBuf::from("/w/main/.git")),
            },
            RootWatch {
                path: PathBuf::from("/w/wt"),
                parent: PathBuf::from("/w"),
                git_dir: Some(PathBuf::from("/w/main/.git/worktrees/wt")),
                common_dir: Some(PathBuf::from("/w/main/.git")),
            },
        ];
        let ignore = crate::engine::build_globs(&[]);
        let is = |p: &str| classify_path(Path::new(p), &roots, &ignore);
        assert!(
            matches!(is("/w/main/.git/worktrees/wt/HEAD"), Scheduled::Head(r) if r == Path::new("/w/wt"))
        );
        assert!(
            matches!(is("/w/main/.git/worktrees/wt/index"), Scheduled::Head(r) if r == Path::new("/w/wt"))
        );
        assert!(
            matches!(is("/w/main/.git/worktrees/wt/logs/HEAD"), Scheduled::Head(r) if r == Path::new("/w/wt"))
        );
        assert!(matches!(is("/w/main/.git/HEAD"), Scheduled::Head(r) if r == Path::new("/w/main")));
        assert!(
            matches!(is("/w/main/.git/refs/heads/main"), Scheduled::Head(r) if r == Path::new("/w/main"))
        );
        assert!(matches!(
            is("/w/main/.git/worktrees/wt/objects/x"),
            Scheduled::Ignore
        ));
        assert!(matches!(is("/w/wt/src/a.rs"), Scheduled::Scan(r) if r == Path::new("/w/wt")));
    }

    /// The handle a caller carries off the watcher fires the watcher's own trigger, and a
    /// request made before the loop comes back around is not lost: `notify_one` leaves a
    /// permit behind. That is what lets deliverable 8 set the depth under the lock first
    /// and ask for the rescan afterwards without a race either way.
    #[tokio::test]
    async fn watcher_rescan_trigger_is_the_loops_own_and_keeps_a_request_made_early() {
        let rescan = Arc::new(Notify::new());
        let trigger = RescanTrigger(Arc::clone(&rescan));
        let far = trigger.clone();
        far.request_rescan();
        tokio::time::timeout(Duration::from_secs(5), rescan.notified())
            .await
            .expect("the request made before the wait is still there");
    }
}
