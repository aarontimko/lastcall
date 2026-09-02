//! The watcher loop (kickoff deliverable 11): one `notify` watcher over every root, git-dir
//! events filtered to an allowlist, trailing-edge debounce, and polling backstops for
//! HEAD (`head_poll`) and root discovery (`rescan`). Filesystem events only *schedule*
//! work; every scan, head inspection and rescan runs on `spawn_blocking` under the
//! engine's mutex, and the outcome is published as an [`EngineEvent`].
//!
//! `ignore_globs` scope the watcher only: an ignored path never wakes a scan, but the next
//! scan (manual, or the `rescan` backstop) still shows the tracked edit.

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

enum Scheduled {
    Scan(PathBuf),
    Head(PathBuf),
    Ignore,
}

fn classify_path(path: &Path, roots: &[RootWatch], ignore: &globset::GlobSet) -> Scheduled {
    // Git-dir hits first: a linked worktree's git dir lives outside its root.
    for r in roots {
        for dir in [&r.git_dir, &r.common_dir].into_iter().flatten() {
            if let Ok(rel) = path.strip_prefix(dir) {
                return if git_dir_allowlisted(rel) {
                    Scheduled::Head(r.path.clone())
                } else {
                    Scheduled::Ignore
                };
            }
        }
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
            git_dir: r.repo.as_ref().map(|_| r.head.git_dir.clone()),
            common_dir: r
                .repo
                .as_ref()
                .filter(|_| r.head.common_dir != r.head.git_dir)
                .map(|_| r.head.common_dir.clone()),
        })
        .collect()
}

/// (Re)install recursive watches: every root, plus git dirs that live outside every root.
fn install_watches(
    watcher: &mut Option<notify::RecommendedWatcher>,
    watched: &mut BTreeSet<PathBuf>,
    roots: &[RootWatch],
) -> Vec<String> {
    let mut notices = Vec::new();
    let Some(w) = watcher.as_mut() else {
        return notices;
    };
    let mut wanted: BTreeSet<PathBuf> = roots.iter().map(|r| r.path.clone()).collect();
    for r in roots {
        for dir in [&r.git_dir, &r.common_dir].into_iter().flatten() {
            if !roots.iter().any(|x| dir.starts_with(&x.path)) {
                wanted.insert(dir.clone());
            }
        }
    }
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

async fn blocking<T: Send + 'static>(
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
    for n in install_watches(&mut watcher, &mut watched, &roots) {
        if !emit(
            &tx,
            EngineEvent::Notice {
                root: None,
                text: n,
            },
        )
        .await
        {
            return;
        }
    }
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
                            for n in install_watches(&mut watcher, &mut watched, &roots) {
                                if !emit(&tx, EngineEvent::Notice { root: None, text: n }).await { return; }
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
                git_dir: Some(PathBuf::from("/w/a/.git")),
                common_dir: None,
            },
            RootWatch {
                path: PathBuf::from("/w/a/inner"),
                git_dir: Some(PathBuf::from("/w/a/inner/.git")),
                common_dir: None,
            },
            RootWatch {
                path: PathBuf::from("/w/wt"),
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
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::config::Config;
    use crate::engine::tests::open_engine;
    use lastcall_testkit::fixture_repo::FixtureRepo;
    use lastcall_testkit::tmp::TempDir;

    async fn next_event(w: &mut Watcher, deadline: Duration) -> Option<EngineEvent> {
        tokio::time::timeout(deadline, w.events.recv())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_commit_without_file_activity_triggers_head_inspection() {
        let repo = FixtureRepo::new("watch-head").unwrap();
        let state = TempDir::new("lc-watch-state");
        let engine = open_engine(&repo, &state, Config::default());
        let mut w = engine.run(EngineTimings {
            debounce: Duration::from_millis(100),
            head_poll: Duration::from_millis(250),
            rescan: Duration::from_secs(60),
        });
        // The initial scan.
        let first = next_event(&mut w, Duration::from_secs(5))
            .await
            .expect("initial pile");
        assert!(matches!(first, EngineEvent::Pile { .. }), "{first:?}");
        repo.git(&["commit", "-q", "--allow-empty", "-m", "empty"])
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut seen = None;
        while tokio::time::Instant::now() < deadline {
            match next_event(&mut w, Duration::from_millis(500)).await {
                Some(EngineEvent::Head { notice, branch, .. }) => {
                    seen = Some((notice, branch));
                    break;
                }
                Some(_) => continue,
                None => continue,
            }
        }
        let (notice, branch) = seen.expect("a Head event within 5s");
        assert_eq!(branch.as_deref(), Some("main"));
        let notice = notice.expect("notice");
        assert!(notice.starts_with("committed on main"), "{notice}");
        w.join().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_ignore_globs_do_not_hide_tracked_edits() {
        let mut repo = FixtureRepo::new("watch-ignore").unwrap();
        repo.commit_files(&[("vendor/x", "v1\n")], "vendor")
            .unwrap();
        let state = TempDir::new("lc-watch-state");
        let engine = open_engine(&repo, &state, Config::default());
        assert!(engine.ignore_globs().is_match("vendor/x"));
        let root = engine.roots()[0].path.clone();
        let mut w = engine.run(EngineTimings {
            debounce: Duration::from_millis(100),
            head_poll: Duration::from_secs(60),
            rescan: Duration::from_millis(1500),
        });
        let first = next_event(&mut w, Duration::from_secs(5))
            .await
            .expect("initial pile");
        assert!(
            matches!(first, EngineEvent::Pile { ref pile, .. } if pile.is_empty()),
            "{first:?}"
        );

        repo.write("vendor/x", "v2\n");
        // Nothing wakes a scan for an ignored path within the debounce window.
        let quiet = next_event(&mut w, Duration::from_millis(700)).await;
        assert!(quiet.is_none(), "ignored path scheduled a scan: {quiet:?}");

        // A manual rescan still shows the tracked edit ...
        let pile = lock(&w.engine).scan(&root).unwrap();
        assert_eq!(crate::scan::pile_lines(&pile), vec!["vendor/x"]);

        // ... and so does the rescan backstop.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut shown = false;
        while tokio::time::Instant::now() < deadline {
            if let Some(EngineEvent::Pile { pile, .. }) =
                next_event(&mut w, Duration::from_millis(500)).await
                && pile.row(b"vendor/x").is_some()
            {
                shown = true;
                break;
            }
        }
        assert!(shown, "the rescan backstop shows the edit");
        w.join().await;
    }
}
