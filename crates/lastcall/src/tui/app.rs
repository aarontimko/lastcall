//! The app state and its reducers (kickoff deliverable 3).
//!
//! `App` is a pure value: `apply` folds engine events into it, `handle` folds user actions,
//! `sync_roots` folds the engine's root list, and `render` (in `render.rs`) is a function of
//! `&App` and the frame area alone. Every field is derived from engine piles and root
//! metadata; nothing here reads a file, a ledger or git (invariant 9), and the same event
//! sequence always yields the same `App`.
//!
//! Selection is by path bytes, never by index: a rescan that reorders or removes rows can
//! only move the selection through [`App::reconcile_selection`]'s documented fallback.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lastcall_engine::engine::{AcceptRequest, Accepted, RootState};
use lastcall_engine::git::Oid;
use lastcall_engine::headstate::InProgress;
use lastcall_engine::hunks::Hunk;
use lastcall_engine::ops::Rendered;
use lastcall_engine::roots::Badge;
use lastcall_engine::scan::{Annotation, Change, Group, Pile, Row};
use lastcall_engine::store::RootKind;
use lastcall_engine::watcher::EngineEvent;

use super::input::{Action, Keymap};

pub const NAV_WIDTH_DEFAULT: u16 = 28;
pub const NAV_WIDTH_MIN: u16 = 16;
pub const NAV_WIDTH_MAX: u16 = 60;
/// Below this many columns the nav is hidden and the diff has focus.
pub const NAV_MIN_COLS: u16 = 70;
/// Below this the frame is the one-line "terminal too small" message.
pub const MIN_SIZE: (u16, u16) = (40, 10);

/// The per-root metadata the nav and the empty state show. Built from a `RootState` under
/// the engine lock (`RootMeta::of`), then owned by the app so rendering never locks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootMeta {
    pub path: PathBuf,
    /// Basename of `path`.
    pub name: String,
    pub kind: RootKind,
    pub parent: PathBuf,
    pub badge: Option<Badge>,
    pub branch: Option<String>,
    pub head: Option<Oid>,
    pub in_progress: Option<InProgress>,
    /// `org/repo` of the origin remote (engine `remote_slug`), `None` when local or absent.
    pub remote: Option<String>,
}

impl RootMeta {
    pub fn of(root: &RootState) -> Self {
        Self {
            path: root.path.clone(),
            name: root.name(),
            kind: root.kind,
            parent: root.parent.clone(),
            badge: root.badge.clone(),
            branch: root.head.branch.clone(),
            head: root.head.head.clone(),
            in_progress: root.head.in_progress,
            remote: root.remote.clone(),
        }
    }

    /// The branch name, else the short head when detached, else `no commits` / `draft`.
    pub fn branch_label(&self) -> String {
        if let Some(b) = &self.branch {
            return b.clone();
        }
        if let Some(h) = &self.head {
            return short(h);
        }
        match self.kind {
            RootKind::Git => "no commits".to_owned(),
            RootKind::Draft => "draft".to_owned(),
        }
    }

    /// `[worktree of x]` / `[nested in x]`, if any.
    pub fn badge_label(&self) -> Option<String> {
        self.badge.as_ref().map(|b| match b {
            Badge::WorktreeOf(p) => format!("[worktree of {}]", basename(p)),
            Badge::NestedIn(p) => format!("[nested in {}]", basename(p)),
        })
    }

    /// `[merge in progress]`, if any.
    pub fn in_progress_label(&self) -> Option<String> {
        self.in_progress
            .map(|ip| format!("[{} in progress]", ip.as_str()))
    }
}

/// One root as the UI sees it: metadata plus the whole last pile (what every accept is
/// built from: `Rendered::of` on a held row, `AcceptRequest::All` on the held pile), with
/// the pile's groups split out for the nav.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootView {
    pub meta: RootMeta,
    /// The last pile applied for this root, exactly as the engine published it.
    pub pile: Pile,
    pub groups: Vec<Group>,
}

impl RootView {
    pub fn new(meta: RootMeta) -> Self {
        Self {
            meta,
            pile: Pile::default(),
            groups: Vec::new(),
        }
    }

    fn set_pile(&mut self, pile: Pile) {
        self.groups = pile.groups();
        self.pile = pile;
    }

    pub fn rows(&self) -> &[Row] {
        &self.pile.rows
    }

    pub fn notices(&self) -> &[String] {
        &self.pile.notices
    }

    /// Listed in the nav iff the pile has rows (kickoff ruling 2). An in-progress operation
    /// is a tag shown on a listed root, never a reason to list or unlist one.
    pub fn listed(&self) -> bool {
        !self.pile.rows.is_empty()
    }

    pub fn row(&self, path: &[u8]) -> Option<&Row> {
        self.pile.rows.iter().find(|r| r.path == path)
    }

    pub fn group(&self, kind: Annotation) -> Option<&Group> {
        self.groups.iter().find(|g| g.kind == kind)
    }
}

/// What the nav has selected, always by root path and row path bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Root(PathBuf),
    Row(PathBuf, Vec<u8>),
    Group(PathBuf, Annotation),
}

impl Selection {
    pub fn root(&self) -> &Path {
        match self {
            Selection::Root(r) | Selection::Row(r, _) | Selection::Group(r, _) => r,
        }
    }
}

/// What the pointer landed on, as resolved by the last render's `HitMap`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    NavRoot(PathBuf),
    NavRow(PathBuf, Vec<u8>),
    NavGroup(PathBuf, Annotation),
    /// The header line of hunk `i` in the diff.
    DiffHunk(usize),
    DiffBody,
    Divider,
    /// The `[Accept All]` control in the header line.
    HeaderAcceptAll,
    /// The `[A accept file]` hint on the main view's header line.
    FileAccept,
    /// The `[a accept]` hint on hunk `i`'s header line.
    HunkAccept(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Nav,
    Diff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffCursor {
    /// Current hunk index (clamped to the row's hunk count).
    pub hunk: usize,
    /// First visible diff line (clamped to the row's line count).
    pub scroll: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub text: String,
    pub at: Instant,
}

/// Whether a reducer step changed anything a frame could show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Changed {
    Yes,
    No,
}

impl Changed {
    fn or(self, other: Changed) -> Changed {
        if self == Changed::Yes || other == Changed::Yes {
            Changed::Yes
        } else {
            Changed::No
        }
    }
}

/// Side effects the loop performs on the app's behalf (the reducer itself does no I/O).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Rescan every root (`Engine::scan_all` under the lock); `refresh_done` clears the flag.
    Refresh,
    /// Re-read `RootMeta::of` for every root and feed it to `sync_roots`.
    SyncRoots,
    Quit,
    /// Run `Engine::accept` for each root covered, all in one critical section, and feed
    /// the results to [`App::accepted`]. Every request is built from the held `RootView`
    /// (§6.3): `Rendered::of` on a held row, `All` on the held pile.
    Accept(Vec<(PathBuf, AcceptRequest)>),
}

/// What one accept covers (§6.7): the key to the status text, the confirm modal's live
/// counts and the advance rule once the pile comes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptScope {
    /// Hunk `index` (0-based) of the `hunks` the row showed when the accept was asked.
    Hunk {
        root: PathBuf,
        path: Vec<u8>,
        index: usize,
        hunks: usize,
    },
    File {
        root: PathBuf,
        path: Vec<u8>,
        deleted: bool,
    },
    Group {
        root: PathBuf,
        kind: Annotation,
    },
    /// Every row of one root.
    Root(PathBuf),
    /// Every row of every listed root.
    All,
}

/// An accept the loop is running: its scope and the rows each request covered, so the
/// status can count what came back `Ok`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepting {
    pub scope: AcceptScope,
    pub files: Vec<(PathBuf, usize)>,
}

/// The confirm modal. Only the scope is stored: the numbers it shows are recomputed from
/// the held piles at every render ([`App::confirm_counts`]), so a pile applied underneath
/// changes them and `Confirm` folds exactly what is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub scope: AcceptScope,
}

/// What a scope covers right now, from the held piles.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfirmCounts {
    pub files: usize,
    /// Rows inside an upstream group.
    pub grouped: usize,
    /// Collapsed rows.
    pub collapsed: usize,
    /// Names of the listed roots covered.
    pub roots: Vec<String>,
}

/// Above this many files an accept asks first (§6.7): 10 accepts, 11 asks.
pub const CONFIRM_ABOVE: usize = 10;
pub const ACCEPT_IN_PROGRESS: &str = "accept in progress";
pub const NOTHING_TO_ACCEPT: &str = "nothing to accept";

/// How long a status notice stays on the status line before the key hints return.
pub const STATUS_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
pub struct App {
    pub roots: BTreeMap<PathBuf, RootView>,
    pub selection: Option<Selection>,
    pub focus: Focus,
    pub diff: DiffCursor,
    /// Outer width of the nav pane (its right border is the divider), 16..=60.
    pub nav_width: u16,
    pub dragging: bool,
    pub full_paths: bool,
    pub show_remote: bool,
    pub help: bool,
    pub status: Option<StatusLine>,
    /// Advanced by `Tick`; render computes ages from it, never from `Instant::now()`.
    pub now: Instant,
    /// Terminal size from the last `Resize`, used for page sizes and the hidden-nav rule
    /// in `handle` (render uses the frame's own area).
    pub size: (u16, u16),
    pub refreshing: bool,
    /// Piles that arrived before `sync_roots` delivered their root's meta; adopted then.
    pub orphan_piles: BTreeMap<PathBuf, Pile>,
    /// The scan seq of the last pile applied per root: an older pile is dropped untouched
    /// (the watcher and a refresh or an accept are two channels; this orders them). The
    /// entry goes when the root does, so a re-added root takes piles from its first scan.
    pub seq: BTreeMap<PathBuf, u64>,
    /// The accept the loop is running, if any; a second one is refused meanwhile.
    pub accepting: Option<Accepting>,
    /// The confirm modal, if open: every action but `Tick`/`Resize`/`Confirm`/`Cancel`/
    /// `Quit` is ignored while it is (`Quit` passes as it does through the help overlay:
    /// `q` and ctrl-c quit by default, everywhere).
    pub confirm: Option<Confirm>,
    /// The effective key bindings, `(action name, key specs)` in `DEFAULT_KEYMAP` order.
    /// Seeded from the defaults; worker 3b replaces it after `Keymap::from_config` so the
    /// hint line and the help overlay show the user's own bindings.
    pub keymap: Vec<(String, Vec<String>)>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            roots: BTreeMap::new(),
            selection: None,
            focus: Focus::Nav,
            diff: DiffCursor::default(),
            nav_width: NAV_WIDTH_DEFAULT,
            dragging: false,
            full_paths: false,
            show_remote: false,
            help: false,
            status: None,
            now: Instant::now(),
            size: (80, 24),
            refreshing: false,
            orphan_piles: BTreeMap::new(),
            seq: BTreeMap::new(),
            accepting: None,
            confirm: None,
            // The canonical spellings (`shift-a` shows as `A`), as `Keymap::table` gives.
            keymap: Keymap::defaults().table(),
        }
    }

    /// The key specs bound to `action` (empty when unbound).
    pub fn keys_for(&self, action: &str) -> &[String] {
        match self.keymap.iter().find(|(n, _)| n == action) {
            Some((_, specs)) => specs.as_slice(),
            None => &[],
        }
    }

    // ---- derived views -------------------------------------------------------------------

    /// Nav order: for each listed root by path, `[Root, rows…, groups…]`.
    pub fn nav_entries(&self) -> Vec<Selection> {
        let mut out = Vec::new();
        for (path, view) in &self.roots {
            if !view.listed() {
                continue;
            }
            out.push(Selection::Root(path.clone()));
            for r in view.rows() {
                out.push(Selection::Row(path.clone(), r.path.clone()));
            }
            for g in &view.groups {
                out.push(Selection::Group(path.clone(), g.kind));
            }
        }
        out
    }

    pub fn listed_roots(&self) -> impl Iterator<Item = &RootView> {
        self.roots.values().filter(|v| v.listed())
    }

    /// The selected row, if the selection is a row that still exists.
    pub fn selected_row(&self) -> Option<&Row> {
        match &self.selection {
            Some(Selection::Row(root, path)) => self.roots.get(root)?.row(path),
            _ => None,
        }
    }

    /// Any listed root whose pile stopped at the engine's row cap (`Pile::omitted`), so the
    /// file counts carry a `+`.
    pub fn any_truncated(&self) -> bool {
        self.listed_roots().any(|v| v.pile.omitted > 0)
    }

    /// The root's display name, from its view when known, else the basename.
    pub fn root_name(&self, root: &Path) -> String {
        self.roots
            .get(root)
            .map(|v| v.meta.name.clone())
            .unwrap_or_else(|| basename(root))
    }

    pub fn nav_visible(&self) -> bool {
        self.size.0 >= NAV_MIN_COLS
    }

    /// `focus`, except that a hidden nav can never hold it.
    pub fn effective_focus(&self) -> Focus {
        if self.nav_visible() {
            self.focus
        } else {
            Focus::Diff
        }
    }

    fn page_rows(&self) -> usize {
        // header + status + the pane's two border rows
        usize::from(self.size.1.saturating_sub(4)).max(1)
    }

    // ---- engine events -------------------------------------------------------------------

    /// Fold one engine event in. `Head` and `RootsChanged` ask the loop for `SyncRoots`.
    pub fn apply(&mut self, event: EngineEvent) -> (Changed, Option<Effect>) {
        match event {
            EngineEvent::Pile { root, seq, pile } => (self.apply_pile(root, seq, pile), None),
            EngineEvent::Head {
                root,
                from,
                to,
                branch,
                notice,
            } => {
                let text = notice.unwrap_or_else(|| {
                    format!(
                        "HEAD {} → {}",
                        from.as_ref().map(short).unwrap_or_else(|| "none".into()),
                        to.as_ref().map(short).unwrap_or_else(|| "none".into())
                    )
                });
                self.set_status(text);
                if let Some(view) = self.roots.get_mut(&root) {
                    view.meta.branch = branch;
                    view.meta.head = to;
                }
                (Changed::Yes, Some(Effect::SyncRoots))
            }
            EngineEvent::RootsChanged(changed) => {
                let mut result = Changed::No;
                for root in &changed.removed {
                    if self.roots.remove(root).is_some() {
                        result = Changed::Yes;
                    }
                    self.orphan_piles.remove(root);
                    self.seq.remove(root);
                }
                if result == Changed::Yes {
                    self.reconcile_selection();
                }
                let effect = (!changed.added.is_empty() || !changed.removed.is_empty())
                    .then_some(Effect::SyncRoots);
                (result, effect)
            }
            EngineEvent::Notice { root, text } => {
                let text = match root.and_then(|r| self.roots.get(&r).map(|v| v.meta.name.clone()))
                {
                    Some(name) => format!("{name}: {text}"),
                    None => text,
                };
                self.set_status(text);
                (Changed::Yes, None)
            }
        }
    }

    /// Fold one pile in. A pile older than the one held for `root` (`seq` below the
    /// remembered one) is dropped untouched; an equal or newer one that changes nothing is
    /// `Changed::No` but still moves the seq forward.
    fn apply_pile(&mut self, root: PathBuf, seq: u64, pile: Pile) -> Changed {
        if self.seq.get(&root).is_some_and(|&held| seq < held) {
            return Changed::No;
        }
        self.seq.insert(root.clone(), seq);
        let Some(view) = self.roots.get_mut(&root) else {
            self.orphan_piles.insert(root, pile);
            return Changed::No;
        };
        if view.pile == pile {
            return Changed::No;
        }
        let fresh = pile
            .notices
            .iter()
            .find(|n| !view.pile.notices.contains(n))
            .cloned();
        view.set_pile(pile);
        if let Some(n) = fresh {
            self.set_status(n);
        }
        self.reconcile_selection();
        Changed::Yes
    }

    /// Replace the root set with the engine's current one: new roots get a `RootView`
    /// (adopting any pile that arrived early), known roots take the new meta, roots the
    /// engine no longer has are dropped.
    pub fn sync_roots(&mut self, metas: Vec<RootMeta>) -> Changed {
        let mut changed = Changed::No;
        let keep: Vec<PathBuf> = metas.iter().map(|m| m.path.clone()).collect();
        for meta in metas {
            match self.roots.get_mut(&meta.path) {
                Some(view) => {
                    if view.meta != meta {
                        view.meta = meta;
                        changed = Changed::Yes;
                    }
                }
                None => {
                    let mut view = RootView::new(meta);
                    if let Some(pile) = self.orphan_piles.remove(&view.meta.path) {
                        view.set_pile(pile);
                    }
                    self.roots.insert(view.meta.path.clone(), view);
                    changed = Changed::Yes;
                }
            }
        }
        let gone: Vec<PathBuf> = self
            .roots
            .keys()
            .filter(|k| !keep.contains(k))
            .cloned()
            .collect();
        for k in gone {
            self.roots.remove(&k);
            self.seq.remove(&k);
            changed = Changed::Yes;
        }
        if changed == Changed::Yes {
            self.reconcile_selection();
        }
        changed
    }

    // ---- accepting -----------------------------------------------------------------------

    /// What `Accept` covers from here: the hunk under the diff cursor when the diff has
    /// focus and the row has hunks, else the selected entry (a row whole, a group, every
    /// row of a root). `None` with nothing selected or a vanished row.
    pub fn accept_scope(&self) -> Option<AcceptScope> {
        match self.selection.clone()? {
            Selection::Row(root, path) => {
                let row = self.roots.get(&root)?.row(&path)?;
                if self.effective_focus() == Focus::Diff && !row.hunks.is_empty() {
                    Some(AcceptScope::Hunk {
                        root,
                        path,
                        index: self.diff.hunk.min(row.hunks.len() - 1),
                        hunks: row.hunks.len(),
                    })
                } else {
                    Some(file_scope(root, row))
                }
            }
            Selection::Group(root, kind) => Some(AcceptScope::Group { root, kind }),
            Selection::Root(root) => Some(AcceptScope::Root(root)),
        }
    }

    /// What `AcceptFile` covers: the selected row whole, whichever pane has focus.
    pub fn accept_file_scope(&self) -> Option<AcceptScope> {
        match self.selection.clone()? {
            Selection::Row(root, path) => {
                let row = self.roots.get(&root)?.row(&path)?;
                Some(file_scope(root, row))
            }
            _ => None,
        }
    }

    /// The requests a scope means right now, one per root covered, each built from the
    /// held `RootView` and nothing else. Empty when the scope no longer covers anything.
    pub fn accept_requests(&self, scope: &AcceptScope) -> Vec<(PathBuf, AcceptRequest)> {
        let mut out = Vec::new();
        match scope {
            AcceptScope::Hunk {
                root, path, index, ..
            } => {
                if let Some(row) = self.roots.get(root).and_then(|v| v.row(path))
                    && *index < row.hunks.len()
                {
                    out.push((
                        root.clone(),
                        AcceptRequest::Hunk {
                            rendered: Rendered::of(row),
                            hunks: row.hunks.clone(),
                            index: *index,
                        },
                    ));
                }
            }
            AcceptScope::File { root, path, .. } => {
                if let Some(row) = self.roots.get(root).and_then(|v| v.row(path)) {
                    out.push((root.clone(), AcceptRequest::File(Rendered::of(row))));
                }
            }
            AcceptScope::Group { root, kind } => {
                let rendered: Vec<Rendered> = self
                    .group_rows(root, *kind)
                    .into_iter()
                    .map(Rendered::of)
                    .collect();
                if !rendered.is_empty() {
                    out.push((root.clone(), AcceptRequest::Group(rendered)));
                }
            }
            AcceptScope::Root(root) => {
                if let Some(view) = self.roots.get(root).filter(|v| v.listed()) {
                    out.push((root.clone(), AcceptRequest::All(view.pile.clone())));
                }
            }
            AcceptScope::All => {
                for (root, view) in &self.roots {
                    if view.listed() {
                        out.push((root.clone(), AcceptRequest::All(view.pile.clone())));
                    }
                }
            }
        }
        out
    }

    /// The rows a scope covers right now, from the held piles.
    pub fn counts_of(&self, scope: &AcceptScope) -> ConfirmCounts {
        let mut counts = ConfirmCounts::default();
        let mut tally = |root: &Path, rows: &[&Row]| {
            if rows.is_empty() {
                return;
            }
            counts.files += rows.len();
            counts.grouped += rows
                .iter()
                .filter(|r| r.annotation == Some(Annotation::Upstream))
                .count();
            counts.collapsed += rows.iter().filter(|r| r.collapsed.is_some()).count();
            counts.roots.push(self.root_name(root));
        };
        match scope {
            AcceptScope::Hunk { root, path, .. } | AcceptScope::File { root, path, .. } => {
                if let Some(row) = self.roots.get(root).and_then(|v| v.row(path)) {
                    tally(root, &[row]);
                }
            }
            AcceptScope::Group { root, kind } => tally(root, &self.group_rows(root, *kind)),
            AcceptScope::Root(root) => {
                if let Some(view) = self.roots.get(root) {
                    tally(root, &view.rows().iter().collect::<Vec<_>>());
                }
            }
            AcceptScope::All => {
                for (root, view) in &self.roots {
                    tally(root, &view.rows().iter().collect::<Vec<_>>());
                }
            }
        }
        counts
    }

    /// The confirm modal's numbers, from the held piles as they are now.
    pub fn confirm_counts(&self) -> Option<ConfirmCounts> {
        self.confirm.as_ref().map(|c| self.counts_of(&c.scope))
    }

    fn group_rows(&self, root: &Path, kind: Annotation) -> Vec<&Row> {
        let Some(view) = self.roots.get(root) else {
            return Vec::new();
        };
        let Some(group) = view.group(kind) else {
            return Vec::new();
        };
        group.paths.iter().filter_map(|p| view.row(p)).collect()
    }

    /// `Accept`/`AcceptFile`/`AcceptAll`: refuse while one runs, ask above
    /// [`CONFIRM_ABOVE`] files, else start.
    fn request_accept(&mut self, scope: AcceptScope) -> (Changed, Option<Effect>) {
        if self.accepting.is_some() {
            self.set_status(ACCEPT_IN_PROGRESS);
            return (Changed::Yes, None);
        }
        let counts = self.counts_of(&scope);
        if counts.files == 0 {
            self.set_status(NOTHING_TO_ACCEPT);
            return (Changed::Yes, None);
        }
        if counts.files > CONFIRM_ABOVE {
            self.confirm = Some(Confirm { scope });
            return (Changed::Yes, None);
        }
        self.start_accept(scope)
    }

    /// Build the requests from the held views and hand them to the loop.
    fn start_accept(&mut self, scope: AcceptScope) -> (Changed, Option<Effect>) {
        let reqs = self.accept_requests(&scope);
        if reqs.is_empty() {
            self.set_status(NOTHING_TO_ACCEPT);
            return (Changed::Yes, None);
        }
        let files = reqs
            .iter()
            .map(|(root, req)| {
                let n = match req {
                    AcceptRequest::Hunk { .. } | AcceptRequest::File(_) => 1,
                    AcceptRequest::Group(rendered) => rendered.len(),
                    AcceptRequest::All(pile) => pile.rows.len(),
                };
                (root.clone(), n)
            })
            .collect();
        self.accepting = Some(Accepting { scope, files });
        self.set_status("accepting…");
        (Changed::Yes, Some(Effect::Accept(reqs)))
    }

    /// The loop's answer to an `Effect::Accept`: every root's pile goes through the same
    /// path as a watcher pile (seq included), then the §6.7 advance rule runs for the
    /// selection the accept was asked from, `accepting` clears and one status line says
    /// what happened. An `Err` for one root is named in the status and undoes nothing.
    pub fn accepted(&mut self, results: Vec<(PathBuf, Result<Accepted, String>)>) -> Changed {
        let inflight = self.accepting.take();
        let before = self.selection.clone();
        let mut changed = if inflight.is_some() {
            Changed::Yes
        } else {
            Changed::No
        };
        let mut refusals: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let mut ok_roots: Vec<PathBuf> = Vec::new();
        for (root, result) in results {
            match result {
                Ok(acc) => {
                    refusals.extend(acc.outcome.refused.iter().map(|r| r.to_string()));
                    ok_roots.push(root.clone());
                    changed = changed.or(self.apply_pile(root, acc.seq, acc.pile));
                }
                Err(e) => errors.push(format!("{}: {e}", self.root_name(&root))),
            }
        }
        let Some(Accepting { scope, files }) = inflight else {
            return changed;
        };
        let taken = refusals.is_empty() && errors.is_empty();
        self.advance_after(&scope, before, taken);
        let accepted: usize = files
            .iter()
            .filter(|(r, _)| ok_roots.contains(r))
            .map(|(_, n)| n)
            .sum();
        let mut parts = Vec::new();
        if refusals.is_empty() && !ok_roots.is_empty() {
            parts.push(self.accepted_text(&scope, accepted, &ok_roots));
        }
        if !refusals.is_empty() {
            parts.push(refusal_text(&refusals));
        }
        parts.extend(errors);
        self.set_status(parts.join(" · "));
        Changed::Yes
    }

    fn accepted_text(&self, scope: &AcceptScope, files: usize, ok_roots: &[PathBuf]) -> String {
        let lossy = |p: &[u8]| String::from_utf8_lossy(p).into_owned();
        match scope {
            AcceptScope::Hunk {
                path, index, hunks, ..
            } => format!("accepted {} · hunk {} of {}", lossy(path), index + 1, hunks),
            AcceptScope::File { path, deleted, .. } => {
                let suffix = if *deleted { " (deleted)" } else { "" };
                format!("accepted {}{suffix}", lossy(path))
            }
            AcceptScope::Group { kind, .. } => format!(
                "accepted {} · {}",
                annotation_name(*kind),
                plural(files, "file")
            ),
            AcceptScope::Root(_) | AcceptScope::All => match ok_roots {
                [one] => format!(
                    "accepted {} in {}",
                    plural(files, "file"),
                    self.root_name(one)
                ),
                many => format!(
                    "accepted {} in {}",
                    plural(files, "file"),
                    plural(many.len(), "repo")
                ),
            },
        }
    }

    /// §6.7 after the piles came back: the selection the accept was asked from is gone →
    /// [`App::advance`] from it; a hunk accept that was `taken` (no refusal, no error) and
    /// left hunks in the row keeps the cursor index (clamped) and scrolls to it. A refused
    /// one moves nothing — the scroll stays where the user had it, not at the hunk header.
    fn advance_after(&mut self, scope: &AcceptScope, before: Option<Selection>, taken: bool) {
        let Some(before) = before else {
            return;
        };
        if !self.nav_entries().contains(&before) {
            let after = match &before {
                Selection::Row(_, p) => Some(p.clone()),
                _ => None,
            };
            let root = before.root().to_path_buf();
            self.advance(&root, after.as_deref());
            return;
        }
        if taken
            && let AcceptScope::Hunk { root, path, .. } = scope
            && before == Selection::Row(root.clone(), path.clone())
            && self.selection == Some(before)
        {
            self.follow_hunk();
        }
    }

    /// Select the next row by path after `after` in `root`'s current pile, else the first
    /// remaining row of that root (wrap), else the next listed root's first row (never its
    /// `Root` entry), else nothing. Focus stays.
    fn advance(&mut self, root: &Path, after: Option<&[u8]>) {
        let entries = self.nav_entries();
        let next = entries
            .iter()
            .find(|e| match e {
                Selection::Row(r, p) => r == root && after.is_none_or(|a| p.as_slice() > a),
                _ => false,
            })
            .or_else(|| {
                entries
                    .iter()
                    .find(|e| matches!(e, Selection::Row(r, _) if r == root))
            })
            .or_else(|| {
                entries
                    .iter()
                    .find(|e| matches!(e, Selection::Row(r, _) if r.as_path() > root))
            })
            .cloned();
        self.select(next);
    }

    /// Scroll so the current hunk's header is the first visible line.
    fn follow_hunk(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let offsets = hunk_offsets(&row.hunks);
        if let Some(&at) = offsets.get(self.diff.hunk) {
            self.diff.scroll = at;
        }
    }

    /// The loop calls this when an `Effect::Refresh` finished (its piles arrive as events).
    pub fn refresh_done(&mut self) -> Changed {
        if !self.refreshing {
            return Changed::No;
        }
        self.refreshing = false;
        self.set_status("refreshed");
        Changed::Yes
    }

    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = Some(StatusLine {
            text: text.into(),
            at: self.now,
        });
    }

    // ---- selection -----------------------------------------------------------------------

    /// Select `next`; a different selection resets the diff cursor, the same one keeps it.
    pub fn select(&mut self, next: Option<Selection>) -> Changed {
        if self.selection == next {
            return Changed::No;
        }
        self.selection = next;
        self.diff = DiffCursor::default();
        Changed::Yes
    }

    /// After roots or piles changed: keep the selection if it still exists (clamping the
    /// diff cursor), else fall through to the next entry by path in the same root, else that
    /// root's `Root` entry, else the next listed root's first entry, else nothing.
    pub fn reconcile_selection(&mut self) {
        let Some(sel) = self.selection.clone() else {
            return;
        };
        let entries = self.nav_entries();
        if entries.contains(&sel) {
            self.clamp_cursor();
            return;
        }
        let root = sel.root().to_path_buf();
        let same_root_next = entries.iter().find(|e| match (&sel, e) {
            (Selection::Row(_, p), Selection::Row(r, q)) => *r == root && q > p,
            (Selection::Row(_, _), Selection::Group(r, _)) => *r == root,
            _ => false,
        });
        let fallback = same_root_next
            .or_else(|| {
                entries
                    .iter()
                    .find(|e| **e == Selection::Root(root.clone()))
            })
            .or_else(|| entries.iter().find(|e| e.root() > root.as_path()))
            .cloned();
        self.select(fallback);
    }

    fn clamp_cursor(&mut self) {
        let Some(row) = self.selected_row() else {
            self.diff = DiffCursor::default();
            return;
        };
        let hunks = row.hunks.len();
        let lines = diff_len(row);
        self.diff.hunk = self.diff.hunk.min(hunks.saturating_sub(1));
        self.diff.scroll = self.diff.scroll.min(lines.saturating_sub(1));
    }

    /// Move the nav cursor `delta` entries (clamped). `handle` uses it for the nav keys;
    /// the loop calls it directly for the wheel over the nav, which moves the selection
    /// whichever pane has focus.
    pub fn move_selection(&mut self, delta: isize) -> Changed {
        let entries = self.nav_entries();
        if entries.is_empty() {
            return self.select(None);
        }
        let last = entries.len() - 1;
        let index = match self
            .selection
            .as_ref()
            .and_then(|s| entries.iter().position(|e| e == s))
        {
            Some(i) => (i as isize + delta).clamp(0, last as isize) as usize,
            None if delta < 0 => last,
            None => 0,
        };
        self.select(Some(entries[index].clone()))
    }

    // ---- diff cursor ---------------------------------------------------------------------

    fn scroll_by(&mut self, delta: isize) -> Changed {
        let Some(row) = self.selected_row() else {
            return Changed::No;
        };
        let max = diff_len(row).saturating_sub(1) as isize;
        let next = (self.diff.scroll as isize + delta).clamp(0, max) as usize;
        if next == self.diff.scroll {
            return Changed::No;
        }
        self.diff.scroll = next;
        Changed::Yes
    }

    /// Move the hunk cursor and scroll so its header is the first visible line.
    fn move_hunk(&mut self, delta: isize) -> Changed {
        let Some(row) = self.selected_row() else {
            return Changed::No;
        };
        if row.hunks.is_empty() {
            return Changed::No;
        }
        let max = row.hunks.len() as isize - 1;
        let next = (self.diff.hunk as isize + delta).clamp(0, max) as usize;
        let offsets = hunk_offsets(&row.hunks);
        let scroll = offsets[next];
        if next == self.diff.hunk && scroll == self.diff.scroll {
            return Changed::No;
        }
        self.diff.hunk = next;
        self.diff.scroll = scroll;
        Changed::Yes
    }

    // ---- user actions --------------------------------------------------------------------

    /// Fold one user action in.
    pub fn handle(&mut self, action: Action) -> (Changed, Option<Effect>) {
        use Action::*;
        if self.confirm.is_some() && !matches!(action, Tick | Resize(..) | Confirm | Cancel | Quit)
        {
            return (Changed::No, None);
        }
        if self.help
            && !matches!(
                action,
                Tick | Resize(..) | Drag(..) | Release | Press(..) | Quit
            )
        {
            self.help = false;
            return (Changed::Yes, None);
        }
        let nav = self.effective_focus() == Focus::Nav;
        let page = self.page_rows() as isize;
        let changed = match action {
            NavUp if nav => self.move_selection(-1),
            NavDown if nav => self.move_selection(1),
            NavPageUp if nav => self.move_selection(-page),
            NavPageDown if nav => self.move_selection(page),
            NavUp => self.scroll_by(-1),
            NavDown => self.scroll_by(1),
            NavPageUp => self.scroll_by(-page),
            NavPageDown => self.scroll_by(page),
            ScrollUp(n) => self.scroll_by(-(n as isize)),
            ScrollDown(n) => self.scroll_by(n as isize),
            Open => match self.selection.clone() {
                Some(Selection::Row(..)) | Some(Selection::Group(..)) => {
                    self.set_focus(Focus::Diff)
                }
                Some(Selection::Root(root)) => {
                    let first = self
                        .nav_entries()
                        .into_iter()
                        .find(|e| matches!(e, Selection::Row(r, _) if *r == root));
                    self.select(first)
                }
                None => self.move_selection(1),
            },
            Back => self.set_focus(Focus::Nav),
            FocusToggle => match self.focus {
                Focus::Nav => self.set_focus(Focus::Diff),
                Focus::Diff => self.set_focus(Focus::Nav),
            },
            HunkNext => self.move_hunk(1),
            HunkPrev => self.move_hunk(-1),
            ToggleFullPaths => {
                self.full_paths = !self.full_paths;
                Changed::Yes
            }
            ToggleRemote => {
                self.show_remote = !self.show_remote;
                Changed::Yes
            }
            Refresh => {
                if self.refreshing {
                    return (Changed::No, None);
                }
                self.refreshing = true;
                self.set_status("refreshing…");
                return (Changed::Yes, Some(Effect::Refresh));
            }
            Help => {
                self.help = true;
                Changed::Yes
            }
            Accept => match self.accept_scope() {
                Some(scope) => return self.request_accept(scope),
                None => Changed::No,
            },
            AcceptFile => match self.accept_file_scope() {
                Some(scope) => return self.request_accept(scope),
                None => Changed::No,
            },
            AcceptAll => return self.request_accept(AcceptScope::All),
            Confirm => match self.confirm.clone() {
                Some(_) if self.accepting.is_some() => {
                    // Re-checked here: the modal stays open, the status says why.
                    self.set_status(ACCEPT_IN_PROGRESS);
                    Changed::Yes
                }
                Some(confirm) => {
                    self.confirm = None;
                    return self.start_accept(confirm.scope);
                }
                None => Changed::No,
            },
            Cancel => {
                if self.confirm.take().is_some() {
                    Changed::Yes
                } else {
                    Changed::No
                }
            }
            Quit => return (Changed::No, Some(Effect::Quit)),
            Press(_, _) => Changed::No,
            Drag(x, _) => {
                if !self.dragging {
                    return (Changed::No, None);
                }
                self.set_nav_width(x.saturating_add(1))
            }
            Release => {
                self.dragging = false;
                Changed::No
            }
            Resize(w, h) => {
                self.size = (w, h);
                Changed::Yes
            }
            Tick => {
                self.now += Duration::from_secs(1);
                match &self.status {
                    Some(s) if self.now.duration_since(s.at) >= STATUS_TTL => {
                        self.status = None; // the hints come back
                        Changed::Yes
                    }
                    Some(_) => Changed::Yes,
                    None => Changed::No,
                }
            }
        };
        (changed, None)
    }

    /// Fold a resolved mouse target in (the loop maps `Press(x, y)` through the `HitMap`).
    pub fn hit(&mut self, target: Target) -> (Changed, Option<Effect>) {
        if self.confirm.is_some() {
            return (Changed::No, None);
        }
        if self.help {
            self.help = false;
            return (Changed::Yes, None);
        }
        let changed = match target {
            Target::HeaderAcceptAll => return self.handle(Action::AcceptAll),
            Target::FileAccept => return self.handle(Action::AcceptFile),
            Target::HunkAccept(i) => {
                // The cursor first lands on hunk `i` exactly as a click on its header
                // does, then the accept is the one `a` would do there.
                self.hit(Target::DiffHunk(i));
                return self.handle(Action::Accept);
            }
            Target::NavRoot(root) => self
                .select(Some(Selection::Root(root)))
                .or(self.set_focus(Focus::Nav)),
            Target::NavRow(root, path) => self
                .select(Some(Selection::Row(root, path)))
                .or(self.set_focus(Focus::Diff)),
            Target::NavGroup(root, kind) => self
                .select(Some(Selection::Group(root, kind)))
                .or(self.set_focus(Focus::Diff)),
            Target::DiffHunk(i) => {
                // Same cursor as `HunkNext`/`HunkPrev` landing on hunk `i`: the header
                // becomes the first visible line, so a click and a key yield equal `App`s.
                let focus = self.set_focus(Focus::Diff);
                let offsets = self
                    .selected_row()
                    .map(|r| hunk_offsets(&r.hunks))
                    .unwrap_or_default();
                if i < offsets.len() && i != self.diff.hunk {
                    self.diff.hunk = i;
                    self.diff.scroll = offsets[i];
                    Changed::Yes
                } else {
                    focus
                }
            }
            Target::DiffBody => self.set_focus(Focus::Diff),
            Target::Divider => {
                self.dragging = true;
                Changed::No
            }
        };
        (changed, None)
    }

    fn set_focus(&mut self, focus: Focus) -> Changed {
        if self.focus == focus {
            return Changed::No;
        }
        self.focus = focus;
        Changed::Yes
    }

    fn set_nav_width(&mut self, width: u16) -> Changed {
        let next = width.clamp(NAV_WIDTH_MIN, NAV_WIDTH_MAX);
        if next == self.nav_width {
            return Changed::No;
        }
        self.nav_width = next;
        Changed::Yes
    }

    /// `3s` / `2m` / `1h` since the status line was set, by the app's clock.
    pub fn status_age(&self) -> Option<String> {
        let s = self.status.as_ref()?;
        let secs = self.now.saturating_duration_since(s.at).as_secs();
        Some(if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m", secs / 60)
        } else {
            format!("{}h", secs / 3600)
        })
    }
}

// ---- diff geometry (shared with render) --------------------------------------------------

/// Lines a hunk occupies in the diff: its header plus its lines; a mode-change hunk is the
/// single line `mode a → b`.
pub fn hunk_height(hunk: &Hunk) -> usize {
    if hunk.is_mode_change() {
        1
    } else {
        1 + hunk.lines.len()
    }
}

/// First diff line of each hunk (its header).
pub fn hunk_offsets(hunks: &[Hunk]) -> Vec<usize> {
    let mut out = Vec::with_capacity(hunks.len());
    let mut at = 0;
    for h in hunks {
        out.push(at);
        at += hunk_height(h);
    }
    out
}

/// Total diff lines of a row.
pub fn diff_len(row: &Row) -> usize {
    row.hunks.iter().map(hunk_height).sum()
}

pub fn short(oid: &Oid) -> String {
    oid.as_str().chars().take(7).collect()
}

/// `1 file`, `2 files`.
pub fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

pub fn annotation_name(a: Annotation) -> &'static str {
    match a {
        Annotation::Upstream => "upstream",
        Annotation::Mixed => "mixed",
    }
}

fn file_scope(root: PathBuf, row: &Row) -> AcceptScope {
    AcceptScope::File {
        root,
        path: row.path.clone(),
        deleted: row.change == Change::Deleted,
    }
}

/// The status text for refusals: one or two joined by ` · `, more as the first plus
/// ` (+N more)`.
pub fn refusal_text(refusals: &[String]) -> String {
    match refusals {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} · {b}"),
        [first, rest @ ..] => format!("{first} (+{} more)", rest.len()),
    }
}

pub fn basename(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

/// Deterministic three-root fixture for the binary crate's unit tests (no git, no files:
/// the piles are the recorded JSON that the engine's serde derives enable).
#[cfg(test)]
pub(crate) mod testfix {
    use super::*;

    /// Piles recorded from `fixture_parent::build` (re-record with the ignored
    /// `record_pile_fixture` test in `tests/test_e2e_tui_snapshots.rs`).
    const PILES: &str = include_str!("../../tests/fixtures/piles_three_roots.json");

    fn piles() -> BTreeMap<String, Pile> {
        serde_json::from_str(PILES).expect("fixture parses")
    }

    pub fn root(name: &str) -> PathBuf {
        PathBuf::from("/W").join(name)
    }

    pub fn meta(name: &str) -> RootMeta {
        let draft = name == "notes";
        RootMeta {
            path: root(name),
            name: name.to_owned(),
            kind: if draft {
                RootKind::Draft
            } else {
                RootKind::Git
            },
            parent: PathBuf::from("/W"),
            badge: None,
            branch: (!draft).then(|| "main".to_owned()),
            head: None,
            in_progress: None,
            remote: None,
        }
    }

    pub fn pile(name: &str) -> Pile {
        piles().remove(name).expect("fixture root")
    }

    pub fn pile_event(name: &str, pile: Pile) -> EngineEvent {
        pile_event_seq(name, 0, pile)
    }

    pub fn pile_event_seq(name: &str, seq: u64, pile: Pile) -> EngineEvent {
        EngineEvent::Pile {
            root: root(name),
            seq,
            pile,
        }
    }

    /// An engine answer for `root`: a clean outcome, `seq`, and `pile` as the rescan.
    pub fn accepted_ok(name: &str, seq: u64, pile: Pile) -> (PathBuf, Result<Accepted, String>) {
        (
            root(name),
            Ok(Accepted {
                outcome: lastcall_engine::ops::Outcome::default(),
                seq,
                pile,
            }),
        )
    }

    /// `pile` without the rows named.
    pub fn without(mut pile: Pile, paths: &[&str]) -> Pile {
        pile.rows
            .retain(|r| !paths.iter().any(|p| r.path == p.as_bytes()));
        pile
    }

    /// alpha (f1, f2), beta (u1, u2, upstream group), notes (n2.md); nothing selected.
    pub fn three_roots() -> App {
        let mut app = App::new();
        assert_eq!(
            app.sync_roots(vec![meta("alpha"), meta("beta"), meta("notes")]),
            Changed::Yes
        );
        for name in ["alpha", "beta", "notes"] {
            assert_eq!(app.apply(pile_event(name, pile(name))).0, Changed::Yes);
        }
        app
    }

    /// alpha's pile with `f1` given a second (cloned) hunk, for hunk-cursor tests.
    pub fn alpha_two_hunks() -> Pile {
        alpha_hunks(2)
    }

    /// alpha's pile with `f1` given `n` (cloned, re-indexed) hunks.
    pub fn alpha_hunks(n: usize) -> Pile {
        let mut p = pile("alpha");
        let first = p.rows[0].hunks[0].clone();
        p.rows[0].hunks = (0..n)
            .map(|i| {
                let mut h = first.clone();
                h.index = i;
                h
            })
            .collect();
        p
    }

    /// A pile of `n` rows `p00`..`pNN` cloned from alpha's `f1`; the first `upstream` of
    /// them annotated `Upstream` (so `groups()` lists them) and the first `collapsed`
    /// of the rest collapsed.
    pub fn rows_n(n: usize, upstream: usize, collapsed: usize) -> Pile {
        let template = pile("alpha").rows[0].clone();
        let mut p = Pile::default();
        for i in 0..n {
            let mut r = template.clone();
            r.path = format!("p{i:02}").into_bytes();
            if i < upstream {
                r.annotation = Some(Annotation::Upstream);
            } else if i < upstream + collapsed {
                r.collapsed = Some(lastcall_engine::scan::Collapsed::Glob);
                r.hunks.clear();
            }
            p.rows.push(r);
        }
        p
    }

    pub fn row(root: &str, path: &str) -> Selection {
        Selection::Row(self::root(root), path.as_bytes().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::testfix::*;
    use super::*;
    use lastcall_engine::roots::RootsChanged;

    fn status(app: &App) -> &str {
        app.status.as_ref().map(|s| s.text.as_str()).unwrap_or("")
    }

    /// The requests of an `Effect::Accept`.
    fn requests(effect: Option<Effect>) -> Vec<(PathBuf, AcceptRequest)> {
        match effect {
            Some(Effect::Accept(reqs)) => reqs,
            other => panic!("expected Effect::Accept, got {other:?}"),
        }
    }

    // ---- accepting (Phase 4b) ----------------------------------------------------------

    #[test]
    fn app_accept_hunk_by_keys_equals_hunk_accept_click() {
        let mut by_key = three_roots();
        by_key.apply(pile_event("alpha", alpha_hunks(3)));
        by_key.select(Some(row("alpha", "f1")));
        by_key.handle(Action::Open);
        let mut by_click = by_key.clone();

        by_key.handle(Action::HunkNext);
        by_key.handle(Action::HunkNext);
        let key_effect = by_key.handle(Action::Accept);
        let click_effect = by_click.hit(Target::HunkAccept(2));
        assert_eq!(
            by_key, by_click,
            "n, n, a and a click on hunk 3's [a accept]"
        );
        assert_eq!(key_effect, click_effect);
        let reqs = requests(key_effect.1);
        let held = by_key.roots[&root("alpha")].row(b"f1").unwrap();
        assert_eq!(
            reqs,
            vec![(
                root("alpha"),
                AcceptRequest::Hunk {
                    rendered: Rendered::of(held),
                    hunks: held.hunks.clone(),
                    index: 2,
                }
            )]
        );
        assert_eq!(
            by_key.accepting,
            Some(Accepting {
                scope: AcceptScope::Hunk {
                    root: root("alpha"),
                    path: b"f1".to_vec(),
                    index: 2,
                    hunks: 3,
                },
                files: vec![(root("alpha"), 1)],
            })
        );
        assert_eq!(status(&by_key), "accepting…");
    }

    #[test]
    fn app_accept_file_by_key_equals_file_accept_click() {
        let mut by_key = three_roots();
        by_key.select(Some(row("alpha", "f1")));
        by_key.handle(Action::Open);
        let mut by_click = by_key.clone();
        let key_effect = by_key.handle(Action::AcceptFile);
        let click_effect = by_click.hit(Target::FileAccept);
        assert_eq!(by_key, by_click);
        assert_eq!(key_effect, click_effect);
        let held = by_key.roots[&root("alpha")].row(b"f1").unwrap();
        assert_eq!(
            requests(key_effect.1.clone()),
            vec![(root("alpha"), AcceptRequest::File(Rendered::of(held)))]
        );
        // With the nav focused, `a` on a row is the file too.
        let mut nav = three_roots();
        nav.select(Some(row("alpha", "f1")));
        assert_eq!(nav.focus, Focus::Nav);
        assert_eq!(nav.handle(Action::Accept).1, key_effect.1);
    }

    #[test]
    fn app_accept_all_by_key_equals_header_click() {
        let mut by_key = three_roots();
        let mut by_click = by_key.clone();
        let key_effect = by_key.handle(Action::AcceptAll);
        let click_effect = by_click.hit(Target::HeaderAcceptAll);
        assert_eq!(by_key, by_click);
        assert_eq!(key_effect, click_effect);
        assert_eq!(
            requests(key_effect.1),
            vec![
                (root("alpha"), AcceptRequest::All(pile("alpha"))),
                (root("beta"), AcceptRequest::All(pile("beta"))),
                (root("notes"), AcceptRequest::All(pile("notes"))),
            ],
            "one All per listed root, carrying exactly the held pile"
        );
        assert_eq!(
            by_key.accepting.as_ref().unwrap().files,
            vec![(root("alpha"), 2), (root("beta"), 2), (root("notes"), 1)]
        );
    }

    #[test]
    fn app_rendered_of_held_row_equals_rendered_of_fixture_row() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f2")));
        let reqs = requests(app.handle(Action::AcceptFile).1);
        let fixture = pile("alpha");
        let fixture_row = fixture.row(b"f2").unwrap();
        assert_eq!(
            reqs,
            vec![(
                root("alpha"),
                AcceptRequest::File(Rendered::of(fixture_row))
            )]
        );
    }

    #[test]
    fn app_accept_on_root_entry_folds_the_held_pile_and_on_group_the_group() {
        let mut app = three_roots();
        app.select(Some(Selection::Root(root("alpha"))));
        assert_eq!(
            requests(app.handle(Action::Accept).1),
            vec![(root("alpha"), AcceptRequest::All(pile("alpha")))]
        );
        assert_eq!(
            app.accepted(vec![accepted_ok("alpha", 2, Pile::default())]),
            Changed::Yes
        );
        assert_eq!(status(&app), "accepted 2 files in alpha");
        assert_eq!(
            app.selection,
            Some(row("beta", "u1")),
            "an emptied root advances to the next listed root's first row"
        );
        assert!(!app.roots[&root("alpha")].listed());

        app.select(Some(Selection::Group(root("beta"), Annotation::Upstream)));
        let beta = pile("beta");
        let group = beta.groups().into_iter().next().unwrap();
        let rendered: Vec<Rendered> = group
            .paths
            .iter()
            .map(|p| Rendered::of(beta.row(p).unwrap()))
            .collect();
        assert_eq!(
            requests(app.handle(Action::Accept).1),
            vec![(root("beta"), AcceptRequest::Group(rendered))]
        );
        let paths: Vec<&str> = group
            .paths
            .iter()
            .map(|p| std::str::from_utf8(p).unwrap())
            .collect();
        app.accepted(vec![accepted_ok("beta", 3, without(beta.clone(), &paths))]);
        assert_eq!(
            status(&app),
            format!("accepted upstream · {}", plural(paths.len(), "file"))
        );
        assert_eq!(
            app.selection,
            Some(row("beta", "u2")),
            "a vanished group advances to the root's first remaining row"
        );
    }

    #[test]
    fn app_older_seq_pile_is_dropped_untouched() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        let newer = without(pile("alpha"), &["f2"]);
        assert_eq!(
            app.apply(pile_event_seq("alpha", 5, newer.clone())).0,
            Changed::Yes
        );
        let held = app.clone();
        assert_eq!(
            app.apply(pile_event_seq("alpha", 3, pile("alpha"))).0,
            Changed::No,
            "seq 3 < held 5"
        );
        assert_eq!(app, held, "an older pile touches nothing, not even the seq");
        assert_eq!(app.seq[&root("alpha")], 5);
        assert_eq!(
            app.apply(pile_event_seq("alpha", 5, newer)).0,
            Changed::No,
            "the same scan again is no change"
        );
        assert_eq!(
            app.apply(pile_event_seq("alpha", 6, pile("alpha"))).0,
            Changed::Yes
        );
        assert_eq!(app.seq[&root("alpha")], 6);
        assert!(app.roots[&root("alpha")].row(b"f2").is_some());
    }

    #[test]
    fn app_removed_root_readded_receives_piles_again() {
        use lastcall_engine::roots::RootsChanged;
        let mut app = three_roots();
        app.apply(pile_event_seq("alpha", 9, pile("alpha")));
        app.apply(EngineEvent::RootsChanged(RootsChanged {
            added: vec![],
            removed: vec![root("alpha")],
        }));
        assert!(
            !app.seq.contains_key(&root("alpha")),
            "the seq goes with the root"
        );
        app.sync_roots(vec![meta("beta"), meta("notes")]);
        app.sync_roots(vec![meta("alpha"), meta("beta"), meta("notes")]);
        assert_eq!(
            app.apply(pile_event_seq("alpha", 1, pile("alpha"))).0,
            Changed::Yes,
            "a re-added root takes its first scan even with a lower seq"
        );
        assert!(app.roots[&root("alpha")].listed());

        // The same through `sync_roots` dropping the root.
        app.apply(pile_event_seq("beta", 9, pile("beta")));
        app.sync_roots(vec![meta("alpha"), meta("notes")]);
        assert!(!app.seq.contains_key(&root("beta")));
        app.sync_roots(vec![meta("alpha"), meta("beta"), meta("notes")]);
        assert_eq!(
            app.apply(pile_event_seq("beta", 2, pile("beta"))).0,
            Changed::Yes
        );
    }

    #[test]
    fn app_second_accept_in_flight_is_refused_and_confirm_keeps_modal() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        assert!(app.handle(Action::AcceptFile).1.is_some());
        let inflight = app.clone();
        assert_eq!(app.handle(Action::Accept), (Changed::Yes, None));
        assert_eq!(status(&app), ACCEPT_IN_PROGRESS);
        assert_eq!(app.handle(Action::AcceptAll), (Changed::Yes, None));
        assert_eq!(app.confirm, None, "refused before the modal opens");
        app.status = inflight.status.clone();
        assert_eq!(app, inflight, "nothing else moved");

        // A modal that is open while an accept runs stays open on Confirm.
        let mut app = three_roots();
        app.apply(pile_event("alpha", rows_n(12, 0, 0)));
        app.select(Some(Selection::Root(root("alpha"))));
        assert_eq!(app.handle(Action::Accept), (Changed::Yes, None));
        assert!(app.confirm.is_some());
        app.accepting = Some(Accepting {
            scope: AcceptScope::File {
                root: root("beta"),
                path: b"u1".to_vec(),
                deleted: false,
            },
            files: vec![(root("beta"), 1)],
        });
        assert_eq!(app.handle(Action::Confirm), (Changed::Yes, None));
        assert!(app.confirm.is_some(), "the modal stays open");
        assert_eq!(status(&app), ACCEPT_IN_PROGRESS);
    }

    #[test]
    fn app_confirm_asks_above_ten_files_not_at_ten() {
        // Only alpha listed, so `ctrl-a` covers exactly its rows.
        let mut ten = three_roots();
        ten.apply(pile_event("beta", Pile::default()));
        ten.apply(pile_event("notes", Pile::default()));
        let mut eleven = ten.clone();
        ten.apply(pile_event("alpha", rows_n(10, 0, 0)));
        eleven.apply(pile_event("alpha", rows_n(11, 1, 2)));

        let (changed, effect) = ten.handle(Action::AcceptAll);
        assert_eq!(changed, Changed::Yes);
        assert_eq!(ten.confirm, None, "10 accepts without asking");
        assert_eq!(
            requests(effect),
            vec![(root("alpha"), AcceptRequest::All(rows_n(10, 0, 0)))]
        );
        ten.accepted(vec![accepted_ok("alpha", 2, Pile::default())]);
        assert_eq!(status(&ten), "accepted 10 files in alpha");
        assert_eq!(ten.selection, None, "nothing listed anywhere → nothing");

        assert_eq!(eleven.handle(Action::AcceptAll), (Changed::Yes, None));
        assert_eq!(
            eleven.confirm,
            Some(Confirm {
                scope: AcceptScope::All
            }),
            "11 asks"
        );
        assert_eq!(
            eleven.confirm_counts(),
            Some(ConfirmCounts {
                files: 11,
                grouped: 1,
                collapsed: 2,
                roots: vec!["alpha".into()],
            })
        );
        assert_eq!(eleven.accepting, None);
    }

    #[test]
    fn app_confirm_counts_follow_a_pile_applied_underneath_and_confirm_folds_it() {
        let mut app = three_roots();
        app.apply(pile_event("beta", Pile::default()));
        app.apply(pile_event("notes", Pile::default()));
        app.apply(pile_event_seq("alpha", 1, rows_n(11, 1, 0)));
        app.select(Some(Selection::Root(root("alpha"))));
        app.handle(Action::Accept);
        assert_eq!(app.confirm_counts().unwrap().files, 11);
        assert_eq!(
            app.apply(pile_event_seq("alpha", 2, rows_n(12, 1, 0))).0,
            Changed::Yes
        );
        assert!(app.confirm.is_some(), "the modal stays open");
        assert_eq!(app.confirm_counts().unwrap().files, 12);
        let (changed, effect) = app.handle(Action::Confirm);
        assert_eq!(changed, Changed::Yes);
        assert_eq!(app.confirm, None);
        assert_eq!(
            requests(effect),
            vec![(root("alpha"), AcceptRequest::All(rows_n(12, 1, 0)))],
            "Confirm folds the pile that is held now"
        );

        // Emptied underneath: Confirm closes the modal with nothing to do.
        let mut app = three_roots();
        app.apply(pile_event_seq("alpha", 1, rows_n(11, 0, 0)));
        app.handle(Action::AcceptAll);
        assert!(app.confirm.is_some());
        for name in ["alpha", "beta", "notes"] {
            app.apply(pile_event_seq(name, 2, Pile::default()));
        }
        assert_eq!(app.handle(Action::Confirm), (Changed::Yes, None));
        assert_eq!(app.confirm, None);
        assert_eq!(status(&app), NOTHING_TO_ACCEPT);
        assert_eq!(app.accepting, None);
    }

    #[test]
    fn app_nothing_to_accept_with_nothing_listed() {
        let mut app = App::new();
        assert_eq!(app.handle(Action::AcceptAll), (Changed::Yes, None));
        assert_eq!(status(&app), NOTHING_TO_ACCEPT);
        assert_eq!(
            app.handle(Action::Accept),
            (Changed::No, None),
            "nothing selected"
        );
        assert_eq!(app.handle(Action::AcceptFile), (Changed::No, None));
        assert_eq!(app.handle(Action::Confirm), (Changed::No, None), "no modal");
        assert_eq!(app.handle(Action::Cancel), (Changed::No, None));
    }

    #[test]
    fn app_cancel_leaves_state_identical_and_the_modal_swallows_the_rest() {
        let mut app = three_roots();
        app.apply(pile_event("alpha", rows_n(11, 0, 0)));
        app.select(Some(row("alpha", "p03")));
        let before = app.clone();
        assert_eq!(app.handle(Action::AcceptAll), (Changed::Yes, None));
        assert!(app.confirm.is_some());
        for action in [
            Action::NavDown,
            Action::Open,
            Action::Accept,
            Action::AcceptFile,
            Action::AcceptAll,
            Action::Refresh,
            Action::Help,
            Action::Back,
        ] {
            assert_eq!(
                app.handle(action),
                (Changed::No, None),
                "{action:?} ignored"
            );
        }
        assert_eq!(
            app.hit(Target::NavRow(root("alpha"), b"p00".to_vec())),
            (Changed::No, None),
            "clicks are ignored too"
        );
        // Quit passes through, as it does through the help overlay; the modal stays.
        assert_eq!(
            app.handle(Action::Quit),
            (Changed::No, Some(Effect::Quit)),
            "q / ctrl-c quit from inside the modal"
        );
        assert!(app.confirm.is_some());
        assert_eq!(app.handle(Action::Resize(120, 40)).0, Changed::Yes);
        app.handle(Action::Resize(80, 24));
        assert_eq!(app.handle(Action::Tick).0, Changed::No);
        app.now = before.now;
        assert_eq!(app.handle(Action::Cancel), (Changed::Yes, None));
        assert_eq!(app, before, "Cancel changes nothing but the modal");
        assert_eq!(app.handle(Action::Cancel), (Changed::No, None));
    }

    #[test]
    fn app_accept_hunk_keeps_cursor_index_and_follows_scroll() {
        let mut app = three_roots();
        app.apply(pile_event_seq("alpha", 1, alpha_hunks(3)));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);
        app.handle(Action::HunkNext);
        assert_eq!(app.diff.hunk, 1);
        app.handle(Action::Accept);
        // The engine rescans: hunks 1 and 3 remain, re-indexed.
        let two = alpha_hunks(2);
        app.accepted(vec![accepted_ok("alpha", 2, two.clone())]);
        assert_eq!(status(&app), "accepted f1 · hunk 2 of 3");
        assert_eq!(app.selection, Some(row("alpha", "f1")));
        assert_eq!(app.diff.hunk, 1, "the next hunk slid into the cursor");
        assert_eq!(app.diff.scroll, hunk_offsets(&two.rows[0].hunks)[1]);
        assert_eq!(app.focus, Focus::Diff);
        assert_eq!(app.accepting, None);

        app.handle(Action::Accept);
        let one = alpha_hunks(1);
        app.accepted(vec![accepted_ok("alpha", 3, one)]);
        assert_eq!(status(&app), "accepted f1 · hunk 2 of 2");
        assert_eq!(app.diff, DiffCursor { hunk: 0, scroll: 0 }, "clamped");

        app.handle(Action::Accept);
        app.accepted(vec![accepted_ok(
            "alpha",
            4,
            without(pile("alpha"), &["f1"]),
        )]);
        assert_eq!(status(&app), "accepted f1 · hunk 1 of 1");
        assert_eq!(
            app.selection,
            Some(row("alpha", "f2")),
            "last hunk → next row"
        );
        assert_eq!(app.focus, Focus::Diff, "focus stays");
    }

    /// A refused hunk accept leaves the diff cursor exactly where it was: the scroll must
    /// not snap back to the hunk header (`follow_hunk` is for a hunk that was taken).
    #[test]
    fn app_refused_hunk_accept_leaves_the_scroll_alone() {
        use lastcall_engine::ops::{Outcome, Refused};
        let mut app = three_roots();
        app.apply(pile_event_seq("alpha", 1, alpha_hunks(3)));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);
        app.handle(Action::HunkNext);
        assert_eq!(app.diff.hunk, 1);
        assert_eq!(app.handle(Action::ScrollDown(2)), (Changed::Yes, None));
        let scrolled = app.diff;
        assert_eq!(
            scrolled.scroll,
            hunk_offsets(&alpha_hunks(3).rows[0].hunks)[1] + 2,
            "two lines past the hunk header"
        );
        assert!(matches!(
            app.handle(Action::Accept).1,
            Some(Effect::Accept(_))
        ));
        // The engine refuses (the baseline moved); its rescan holds the same three hunks.
        app.accepted(vec![(
            root("alpha"),
            Ok(Accepted {
                outcome: Outcome {
                    refused: vec![Refused::BaselineMoved {
                        path: b"f1".to_vec(),
                    }],
                    ..Outcome::default()
                },
                seq: 2,
                pile: alpha_hunks(3),
            }),
        )]);
        assert!(status(&app).starts_with("f1:"), "{}", status(&app));
        assert_eq!(app.selection, Some(row("alpha", "f1")));
        assert_eq!(app.accepting, None);
        assert_eq!(app.diff, scrolled, "a refused hunk accept moves nothing");
    }

    #[test]
    fn app_advance_to_next_row_by_path_when_not_last() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::AcceptFile);
        app.accepted(vec![accepted_ok(
            "alpha",
            2,
            without(pile("alpha"), &["f1"]),
        )]);
        assert_eq!(status(&app), "accepted f1");
        assert_eq!(app.selection, Some(row("alpha", "f2")));
        assert_eq!(app.focus, Focus::Nav);
    }

    #[test]
    fn app_advance_wraps_to_first_remaining_row_when_last_by_path() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f2")));
        app.handle(Action::AcceptFile);
        app.accepted(vec![accepted_ok(
            "alpha",
            2,
            without(pile("alpha"), &["f2"]),
        )]);
        assert_eq!(status(&app), "accepted f2");
        assert_eq!(
            app.selection,
            Some(row("alpha", "f1")),
            "wraps, not the Root entry"
        );
    }

    #[test]
    fn app_advance_to_next_root_first_row_when_root_empties() {
        let mut app = three_roots();
        app.apply(pile_event("alpha", without(pile("alpha"), &["f2"])));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::AcceptFile);
        app.accepted(vec![accepted_ok("alpha", 2, Pile::default())]);
        assert_eq!(
            app.selection,
            Some(row("beta", "u1")),
            "the row, not beta's Root entry"
        );
    }

    #[test]
    fn app_advance_to_nothing_when_the_only_root_empties() {
        let mut app = three_roots();
        app.apply(pile_event("beta", Pile::default()));
        app.apply(pile_event("notes", Pile::default()));
        app.select(Some(row("alpha", "f2")));
        app.handle(Action::AcceptFile);
        app.accepted(vec![accepted_ok(
            "alpha",
            2,
            without(pile("alpha"), &["f2"]),
        )]);
        assert_eq!(app.selection, Some(row("alpha", "f1")));
        app.handle(Action::AcceptFile);
        app.accepted(vec![accepted_ok("alpha", 3, Pile::default())]);
        assert_eq!(app.selection, None);
        assert_eq!(status(&app), "accepted f1");
    }

    #[test]
    fn app_accepted_deleted_file_says_so() {
        let mut app = three_roots();
        let mut p = pile("alpha");
        p.rows[0].change = Change::Deleted;
        app.apply(pile_event("alpha", p));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::AcceptFile);
        app.accepted(vec![accepted_ok(
            "alpha",
            2,
            without(pile("alpha"), &["f1"]),
        )]);
        assert_eq!(status(&app), "accepted f1 (deleted)");
    }

    #[test]
    fn app_multi_root_accepted_with_one_err_applies_others_and_names_failed_root() {
        let mut app = three_roots();
        app.select(Some(row("beta", "u1")));
        app.handle(Action::AcceptAll);
        let changed = app.accepted(vec![
            (root("alpha"), Err("boom".into())),
            accepted_ok("beta", 2, Pile::default()),
            accepted_ok("notes", 2, Pile::default()),
        ]);
        assert_eq!(changed, Changed::Yes);
        assert!(
            app.roots[&root("alpha")].listed(),
            "alpha's pile is untouched"
        );
        assert!(!app.roots[&root("beta")].listed());
        assert!(!app.roots[&root("notes")].listed());
        assert_eq!(status(&app), "accepted 3 files in 2 repos · alpha: boom");
        assert_eq!(app.accepting, None);
        assert_eq!(
            app.selection, None,
            "beta emptied with no listed root after it → nothing"
        );

        // All three fine: one repo count, or the root's name when it is one.
        let mut app = three_roots();
        app.handle(Action::AcceptAll);
        app.accepted(vec![
            accepted_ok("alpha", 2, Pile::default()),
            accepted_ok("beta", 2, Pile::default()),
            accepted_ok("notes", 2, Pile::default()),
        ]);
        assert_eq!(status(&app), "accepted 5 files in 3 repos");
    }

    #[test]
    fn app_refusal_text_joins_one_two_and_five() {
        use lastcall_engine::ops::{Outcome, Refused};
        let text = |n: usize| -> Vec<String> {
            (0..n)
                .map(|i| {
                    Refused::Moved {
                        path: format!("f{i}").into_bytes(),
                        live: None,
                    }
                    .to_string()
                })
                .collect()
        };
        assert_eq!(
            refusal_text(&text(1)),
            "f0: changed since rendered; not accepted"
        );
        assert_eq!(
            refusal_text(&text(2)),
            "f0: changed since rendered; not accepted · f1: changed since rendered; not accepted"
        );
        assert_eq!(
            refusal_text(&text(5)),
            "f0: changed since rendered; not accepted (+4 more)"
        );
        assert_eq!(refusal_text(&[]), "");

        // Through `accepted`: the row stays (with the rescan's counts), no advance.
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::AcceptFile);
        let mut rescanned = pile("alpha");
        rescanned.rows[0].added = 7;
        app.accepted(vec![(
            root("alpha"),
            Ok(Accepted {
                outcome: Outcome {
                    refused: vec![Refused::Moved {
                        path: b"f1".to_vec(),
                        live: None,
                    }],
                    ..Outcome::default()
                },
                seq: 2,
                pile: rescanned,
            }),
        )]);
        assert_eq!(status(&app), "f1: changed since rendered; not accepted");
        assert_eq!(app.selection, Some(row("alpha", "f1")));
        assert_eq!(app.roots[&root("alpha")].row(b"f1").unwrap().added, 7);
        assert_eq!(app.accepting, None);
    }

    #[test]
    fn app_stray_accepted_applies_piles_without_a_status() {
        let mut app = three_roots();
        app.status = None;
        assert_eq!(
            app.accepted(vec![accepted_ok(
                "alpha",
                2,
                without(pile("alpha"), &["f1"])
            )]),
            Changed::Yes
        );
        assert_eq!(app.status, None);
        assert_eq!(app.seq[&root("alpha")], 2);
    }

    #[test]
    fn app_fixture_lists_three_roots_in_nav_order() {
        let app = three_roots();
        let entries = app.nav_entries();
        assert_eq!(
            entries,
            vec![
                Selection::Root(root("alpha")),
                row("alpha", "f1"),
                row("alpha", "f2"),
                Selection::Root(root("beta")),
                row("beta", "u1"),
                row("beta", "u2"),
                Selection::Group(root("beta"), Annotation::Upstream),
                Selection::Root(root("notes")),
                row("notes", "n2.md"),
            ]
        );
        assert_eq!(app.selection, None);
    }

    #[test]
    fn app_unchanged_pile_is_no_change() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::HunkNext);
        let before = app.clone();
        assert_eq!(app.apply(pile_event("alpha", pile("alpha"))).0, Changed::No);
        assert_eq!(app, before);
    }

    #[test]
    fn app_selection_falls_through_when_row_vanishes() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        let mut p = pile("alpha");
        p.rows.retain(|r| r.path != b"f1");
        app.apply(pile_event("alpha", p));
        assert_eq!(app.selection, Some(row("alpha", "f2")), "next row by path");

        app.apply(pile_event("alpha", Pile::default()));
        assert_eq!(
            app.selection,
            Some(Selection::Root(root("beta"))),
            "root unlisted → next listed root's first entry"
        );

        app.select(Some(Selection::Group(root("beta"), Annotation::Upstream)));
        let mut p = pile("beta");
        for r in &mut p.rows {
            r.annotation = Some(Annotation::Mixed);
        }
        app.apply(pile_event("beta", p));
        assert_eq!(
            app.selection,
            Some(Selection::Root(root("beta"))),
            "vanished group → its root entry"
        );

        app.select(Some(row("notes", "n2.md")));
        app.apply(pile_event("notes", Pile::default()));
        assert_eq!(app.selection, None, "no root after the last one → nothing");
    }

    #[test]
    fn app_last_row_falls_to_root_entry() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f2")));
        let mut p = pile("alpha");
        p.rows.retain(|r| r.path != b"f2");
        app.apply(pile_event("alpha", p));
        assert_eq!(app.selection, Some(Selection::Root(root("alpha"))));
    }

    #[test]
    fn app_hunk_cursor_clamps_when_pile_shrinks_row() {
        let mut app = three_roots();
        let p = alpha_two_hunks();
        app.apply(pile_event("alpha", p.clone()));
        app.select(Some(row("alpha", "f1")));
        assert_eq!(app.handle(Action::HunkNext).0, Changed::Yes);
        assert_eq!(app.diff.hunk, 1);
        assert_eq!(app.diff.scroll, hunk_offsets(&p.rows[0].hunks)[1]);
        assert_eq!(
            app.handle(Action::HunkNext).0,
            Changed::No,
            "clamped at the end"
        );

        app.apply(pile_event("alpha", pile("alpha")));
        assert_eq!(app.selection, Some(row("alpha", "f1")));
        assert_eq!(app.diff.hunk, 0);
        assert!(app.diff.scroll < diff_len(&pile("alpha").rows[0]));
    }

    #[test]
    fn app_root_hidden_when_pile_empty() {
        let mut app = three_roots();
        assert!(app.nav_entries().iter().any(|e| e.root() == root("beta")));
        app.apply(pile_event("beta", Pile::default()));
        assert!(app.nav_entries().iter().all(|e| e.root() != root("beta")));
        assert!(app.roots.contains_key(&root("beta")), "the view stays");
    }

    #[test]
    fn app_in_progress_tag_never_lists_or_unlists_a_root() {
        let mut app = three_roots();
        // beta's pile is empty → hidden; the tag alone must not list it.
        app.apply(pile_event("beta", Pile::default()));
        let mut alpha = meta("alpha");
        alpha.in_progress = Some(InProgress::Merge);
        let mut beta = meta("beta");
        beta.in_progress = Some(InProgress::Rebase);
        assert_eq!(
            app.sync_roots(vec![alpha, beta, meta("notes")]),
            Changed::Yes
        );
        assert!(
            app.nav_entries().iter().all(|e| e.root() != root("beta")),
            "empty pile + tag stays hidden"
        );
        assert!(!app.roots[&root("beta")].listed());
        // alpha has rows → still listed, tag decorates it.
        assert!(app.roots[&root("alpha")].listed());
        assert_eq!(app.nav_entries()[0], Selection::Root(root("alpha")));
        assert_eq!(
            app.roots[&root("alpha")]
                .meta
                .in_progress_label()
                .as_deref(),
            Some("[merge in progress]")
        );
    }

    #[test]
    fn app_hunk_click_equals_hunk_key() {
        let mut by_key = three_roots();
        by_key.apply(pile_event("alpha", alpha_two_hunks()));
        by_key.select(Some(row("alpha", "f1")));
        by_key.handle(Action::Open);
        assert_eq!(by_key.diff, DiffCursor::default());
        let mut by_click = by_key.clone();

        assert_eq!(by_key.handle(Action::HunkNext).0, Changed::Yes);
        assert_eq!(by_click.hit(Target::DiffHunk(1)).0, Changed::Yes);
        assert_eq!(by_key, by_click, "n and a click on hunk 2 yield equal Apps");
        assert_eq!(by_key.diff.hunk, 1);
        assert!(by_key.diff.scroll > 0, "scrolled so the header is visible");

        assert_eq!(
            by_click.hit(Target::DiffHunk(1)).0,
            Changed::No,
            "same hunk"
        );
        assert_eq!(
            by_click.hit(Target::DiffHunk(7)).0,
            Changed::No,
            "out of range"
        );
        assert_eq!(by_key, by_click);
    }

    #[test]
    fn app_nav_width_clamps() {
        let mut app = App::new();
        assert_eq!(app.nav_width, 28);
        assert_eq!(
            app.handle(Action::Drag(50, 3)).0,
            Changed::No,
            "not dragging"
        );
        app.hit(Target::Divider);
        assert!(app.dragging);
        app.handle(Action::Drag(5, 3));
        assert_eq!(app.nav_width, NAV_WIDTH_MIN);
        app.handle(Action::Drag(200, 3));
        assert_eq!(app.nav_width, NAV_WIDTH_MAX);
        app.handle(Action::Drag(39, 3));
        assert_eq!(app.nav_width, 40, "divider column x → outer width x + 1");
        app.handle(Action::Release);
        assert!(!app.dragging);
    }

    #[test]
    fn app_tick_is_no_change_without_a_status_line() {
        let mut app = three_roots();
        let t0 = app.now;
        assert_eq!(app.handle(Action::Tick).0, Changed::No);
        assert_eq!(app.now, t0 + Duration::from_secs(1));
        app.set_status("hello");
        assert_eq!(app.handle(Action::Tick).0, Changed::Yes);
        assert_eq!(app.status_age().as_deref(), Some("1s"));
        // formatting past a minute, without waiting for the TTL
        app.status.as_mut().unwrap().at = app.now - Duration::from_secs(60);
        assert_eq!(app.status_age().as_deref(), Some("1m"));
    }

    #[test]
    fn app_status_line_expires_after_the_ttl_and_the_hints_return() {
        let mut app = three_roots();
        app.set_status("watching /somewhere (3 roots)");
        let ttl = STATUS_TTL.as_secs();
        for _ in 0..ttl - 1 {
            assert_eq!(app.handle(Action::Tick).0, Changed::Yes);
        }
        assert!(app.status.is_some(), "still shown one tick before the TTL");
        assert_eq!(app.handle(Action::Tick), (Changed::Yes, None));
        assert_eq!(app.status, None, "cleared at the TTL");
        assert_eq!(app.handle(Action::Tick).0, Changed::No);
        app.set_status("again");
        assert!(app.status.is_some(), "a new notice starts a new TTL");
    }

    #[test]
    fn app_head_event_sets_status_updates_meta_and_syncs() {
        let mut app = three_roots();
        let to = Oid::parse("d4e5f6a000000000000000000000000000000000").unwrap();
        let (changed, effect) = app.apply(EngineEvent::Head {
            root: root("alpha"),
            from: Oid::parse("a1b2c3d000000000000000000000000000000000"),
            to: Some(to.clone()),
            branch: Some("feature".into()),
            notice: None,
        });
        assert_eq!((changed, effect), (Changed::Yes, Some(Effect::SyncRoots)));
        assert_eq!(app.status.as_ref().unwrap().text, "HEAD a1b2c3d → d4e5f6a");
        assert_eq!(
            app.roots[&root("alpha")].meta.branch.as_deref(),
            Some("feature")
        );
        assert_eq!(app.roots[&root("alpha")].meta.head, Some(to.clone()));

        app.apply(EngineEvent::Head {
            root: root("alpha"),
            from: None,
            to: Some(to),
            branch: Some("main".into()),
            notice: Some("committed on main (1 commit)".into()),
        });
        assert_eq!(
            app.status.as_ref().unwrap().text,
            "committed on main (1 commit)"
        );
    }

    #[test]
    fn app_roots_changed_drops_removed_and_adopts_orphan_piles_on_sync() {
        let mut app = three_roots();
        app.select(Some(row("beta", "u1")));
        let (changed, effect) = app.apply(EngineEvent::RootsChanged(RootsChanged {
            added: vec![root("gamma")],
            removed: vec![root("beta")],
        }));
        assert_eq!((changed, effect), (Changed::Yes, Some(Effect::SyncRoots)));
        assert!(!app.roots.contains_key(&root("beta")));
        assert_eq!(app.selection, Some(Selection::Root(root("notes"))));

        // gamma's pile arrives before its meta: held, not shown.
        assert_eq!(app.apply(pile_event("gamma", pile("alpha"))).0, Changed::No);
        assert!(!app.roots.contains_key(&root("gamma")));
        assert_eq!(
            app.sync_roots(vec![meta("alpha"), meta("gamma"), meta("notes")]),
            Changed::Yes
        );
        assert!(app.roots[&root("gamma")].listed());
        assert!(app.orphan_piles.is_empty());

        // An unchanged root list is a no-op.
        assert_eq!(
            app.sync_roots(vec![meta("alpha"), meta("gamma"), meta("notes")]),
            Changed::No
        );
    }

    #[test]
    fn app_new_root_is_listed_only_after_its_first_pile() {
        let mut app = three_roots();
        app.sync_roots(vec![
            meta("alpha"),
            meta("beta"),
            meta("notes"),
            meta("delta"),
        ]);
        assert!(app.roots.contains_key(&root("delta")));
        assert!(!app.roots[&root("delta")].listed());
        app.apply(pile_event("delta", pile("notes")));
        assert!(app.roots[&root("delta")].listed());
    }

    #[test]
    fn app_reselecting_same_row_keeps_hunk_but_a_different_row_resets() {
        let mut app = three_roots();
        app.apply(pile_event("alpha", alpha_two_hunks()));
        app.hit(Target::NavRow(root("alpha"), b"f1".to_vec()));
        assert_eq!(app.focus, Focus::Diff, "a click on a row focuses the diff");
        app.handle(Action::HunkNext);
        assert_eq!(app.diff.hunk, 1);
        app.hit(Target::NavRow(root("alpha"), b"f1".to_vec()));
        assert_eq!(app.diff.hunk, 1, "same path keeps the cursor");
        app.hit(Target::NavRow(root("alpha"), b"f2".to_vec()));
        assert_eq!(app.diff, DiffCursor::default(), "different row resets");
    }

    #[test]
    fn app_nav_keys_walk_entries_and_diff_keys_scroll() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        assert_eq!(app.handle(Action::NavDown).0, Changed::Yes);
        assert_eq!(app.selection, Some(Selection::Root(root("alpha"))));
        app.handle(Action::NavDown);
        assert_eq!(app.selection, Some(row("alpha", "f1")));
        assert_eq!(app.handle(Action::NavUp).0, Changed::Yes);
        app.handle(Action::NavUp);
        assert_eq!(
            app.handle(Action::NavUp).0,
            Changed::No,
            "clamped at the top"
        );
        app.handle(Action::NavPageDown);
        assert_eq!(
            app.selection,
            Some(row("notes", "n2.md")),
            "page past the end clamps"
        );
        app.handle(Action::NavPageUp);
        assert_eq!(app.selection, Some(Selection::Root(root("alpha"))));

        // Open on a root selects its first row; Open on a row focuses the diff.
        app.handle(Action::Open);
        assert_eq!(app.selection, Some(row("alpha", "f1")));
        assert_eq!(app.focus, Focus::Nav);
        app.handle(Action::Open);
        assert_eq!(app.focus, Focus::Diff);
        assert_eq!(
            app.handle(Action::NavDown).0,
            Changed::Yes,
            "scrolls the diff"
        );
        assert_eq!(app.diff.scroll, 1);
        assert_eq!(
            app.selection,
            Some(row("alpha", "f1")),
            "selection untouched"
        );
        app.handle(Action::ScrollUp(5));
        assert_eq!(app.diff.scroll, 0);
        assert_eq!(app.handle(Action::Back).0, Changed::Yes);
        assert_eq!(app.focus, Focus::Nav);
        assert_eq!(app.handle(Action::Back).0, Changed::No, "Back never quits");
        app.handle(Action::FocusToggle);
        assert_eq!(app.focus, Focus::Diff);
    }

    #[test]
    fn app_narrow_terminal_forces_diff_focus_for_keys() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Resize(60, 20));
        assert_eq!(app.effective_focus(), Focus::Diff);
        app.handle(Action::NavDown);
        assert_eq!(app.selection, Some(row("alpha", "f1")));
        assert_eq!(app.diff.scroll, 1);
    }

    #[test]
    fn app_help_opens_and_any_key_closes_it() {
        let mut app = three_roots();
        assert_eq!(app.handle(Action::Help).0, Changed::Yes);
        assert!(app.help);
        assert_eq!(app.handle(Action::Tick).0, Changed::No);
        assert!(app.help, "the timer does not close help");
        assert_eq!(app.handle(Action::NavDown).0, Changed::Yes);
        assert!(!app.help);
        assert_eq!(app.selection, None, "the closing key is consumed");
        app.handle(Action::Help);
        assert_eq!(app.handle(Action::Quit), (Changed::No, Some(Effect::Quit)));
    }

    #[test]
    fn app_refresh_is_an_effect_and_ignored_while_running() {
        let mut app = three_roots();
        assert_eq!(
            app.handle(Action::Refresh),
            (Changed::Yes, Some(Effect::Refresh))
        );
        assert!(app.refreshing);
        assert_eq!(app.status.as_ref().unwrap().text, "refreshing…");
        assert_eq!(app.handle(Action::Refresh), (Changed::No, None));
        assert_eq!(app.refresh_done(), Changed::Yes);
        assert!(!app.refreshing);
        assert_eq!(app.refresh_done(), Changed::No);
    }

    #[test]
    fn app_notice_event_sets_status_with_root_name() {
        let mut app = three_roots();
        app.apply(EngineEvent::Notice {
            root: Some(root("beta")),
            text: "index rebuilt".into(),
        });
        assert_eq!(app.status.as_ref().unwrap().text, "beta: index rebuilt");
        app.apply(EngineEvent::Notice {
            root: None,
            text: "watcher restarted".into(),
        });
        assert_eq!(app.status.as_ref().unwrap().text, "watcher restarted");
    }

    #[test]
    fn app_fresh_pile_notice_sets_status() {
        let mut app = three_roots();
        let mut p = pile("alpha");
        p.notices.push("f9: unreadable".into());
        app.apply(pile_event("alpha", p.clone()));
        assert_eq!(app.status.as_ref().unwrap().text, "f9: unreadable");
        app.status = None;
        assert_eq!(app.apply(pile_event("alpha", p)).0, Changed::No);
        assert_eq!(app.status, None, "an old notice does not re-announce");
    }

    #[test]
    fn app_branch_label_falls_back_like_status() {
        let mut m = meta("alpha");
        assert_eq!(m.branch_label(), "main");
        m.branch = None;
        m.head = Oid::parse("0123456789abcdef0123456789abcdef01234567");
        assert_eq!(m.branch_label(), "0123456");
        m.head = None;
        assert_eq!(m.branch_label(), "no commits");
        assert_eq!(meta("notes").branch_label(), "draft");
        m.badge = Some(Badge::WorktreeOf(PathBuf::from("/W/alpha-main")));
        assert_eq!(m.badge_label().as_deref(), Some("[worktree of alpha-main]"));
    }

    #[test]
    fn app_diff_geometry_counts_mode_hunks_as_one_line() {
        let p = pile("alpha");
        let row = &p.rows[0];
        assert_eq!(diff_len(row), 1 + row.hunks[0].lines.len());
        let mut mode = row.hunks[0].clone();
        mode.old_range = 0..0;
        mode.new_range = 0..0;
        mode.lines = vec![
            (lastcall_engine::hunks::Tag::Delete, b"mode 100644".to_vec()),
            (lastcall_engine::hunks::Tag::Insert, b"mode 100755".to_vec()),
        ];
        assert!(mode.is_mode_change());
        assert_eq!(hunk_height(&mode), 1);
        assert_eq!(
            hunk_offsets(&[mode.clone(), row.hunks[0].clone()]),
            vec![0, 1]
        );
    }
}
