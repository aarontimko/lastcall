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
use tokio::sync::{mpsc, watch};
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
    /// Backstop: re-inspect every git root's HEAD this often.
    pub head_poll: Duration,
    /// Backstop: re-run discovery and scan every root this often.
    pub rescan: Duration,
}

impl Default for EngineTimings {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(750),
            head_poll: Duration::from_secs(10),
            rescan: Duration::from_secs(30),
        }
    }
}

/// What the watcher publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    /// A root was scanned.
    Pile {
        root: PathBuf,
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
}

/// The running watcher: the event stream, the shared engine, and a stop switch.
pub struct Watcher {
    pub events: mpsc::Receiver<EngineEvent>,
    pub engine: Arc<Mutex<Engine>>,
    pub handle: JoinHandle<()>,
    stop: watch::Sender<bool>,
}

impl Watcher {
    /// Ask the loop to finish; `handle` completes shortly after.
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }

    pub async fn join(self) {
        let _ = self.stop.send(true);
        let _ = self.handle.await;
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
        let handle = tokio::spawn(run_loop(engine.clone(), timings, tx, stop_rx));
        Watcher {
            events,
            engine,
            handle,
            stop,
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
    match blocking(engine, move |e| e.scan(&r)).await {
        Ok(pile) => emit(tx, EngineEvent::Pile { root, pile }).await,
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

async fn inspect_root(
    engine: &Arc<Mutex<Engine>>,
    tx: &mpsc::Sender<EngineEvent>,
    root: PathBuf,
) -> bool {
    let r = root.clone();
    match blocking(engine, move |e| e.inspect_head(&r)).await {
        Ok(Some(HeadChange {
            root,
            from,
            to,
            branch,
            notice,
            pile,
        })) => {
            emit(
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
                && emit(tx, EngineEvent::Pile { root, pile }).await
        }
        Ok(None) => true,
        Err(e) => {
            emit(
                tx,
                EngineEvent::Notice {
                    root: Some(root),
                    text: format!("head inspection failed: {e}"),
                },
            )
            .await
        }
    }
}

async fn run_loop(
    engine: Arc<Mutex<Engine>>,
    timings: EngineTimings,
    tx: mpsc::Sender<EngineEvent>,
    mut stop: watch::Receiver<bool>,
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
    // Initial scans.
    for r in roots.iter().map(|r| r.path.clone()).collect::<Vec<_>>() {
        if !scan_root(&engine, &tx, r).await {
            return;
        }
    }

    let mut due: BTreeMap<PathBuf, Instant> = BTreeMap::new();
    let mut head_due: BTreeSet<PathBuf> = BTreeSet::new();
    let mut head_poll = tokio::time::interval(timings.head_poll);
    head_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    head_poll.tick().await; // the immediate first tick
    let mut rescan = tokio::time::interval(timings.rescan);
    rescan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    rescan.tick().await;

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
                                    inspect_root(&engine, &tx, r.path).await
                                } else {
                                    scan_root(&engine, &tx, r.path).await
                                };
                                if !ok { return; }
                            }
                            rescan.reset();
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
                        for r in &roots { due.insert(r.path.clone(), now); }
                    }
                    Ok(ev) => {
                        if ev.need_rescan() {
                            let now = Instant::now();
                            for r in &roots { due.insert(r.path.clone(), now); }
                        }
                        if actionable(&ev.kind) {
                            for p in &ev.paths {
                                match classify_path(p, &roots, &ignore) {
                                    Scheduled::Scan(root) => { due.insert(root, Instant::now() + timings.debounce); }
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
            _ = rescan.tick() => {
                let changed = blocking(&engine, |e| e.rescan()).await;
                match changed {
                    Ok(changed) => {
                        if !changed.is_empty() {
                            roots = { let g = lock(&engine); root_watches(&g) };
                            if install.is_some() {
                                reinstall = true;
                            } else {
                                install = Some(spawn_install(watcher.take(), std::mem::take(&mut watched), roots.clone()));
                            }
                            if !emit(&tx, EngineEvent::RootsChanged(changed)).await { return; }
                        }
                    }
                    Err(e) => {
                        if !emit(&tx, EngineEvent::Notice { root: None, text: format!("rescan failed: {e}") }).await { return; }
                    }
                }
                let now = Instant::now();
                for r in &roots { due.insert(r.path.clone(), now); }
            }
            _ = sleep, if next_due.is_some() => {}
        }

        // Drain: head inspections first (they scan too), then due scans.
        for root in std::mem::take(&mut head_due) {
            if !inspect_root(&engine, &tx, root).await {
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
            if !scan_root(&engine, &tx, root).await {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
