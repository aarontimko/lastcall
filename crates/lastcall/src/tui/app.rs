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

use lastcall_engine::engine::RootState;
use lastcall_engine::git::Oid;
use lastcall_engine::headstate::InProgress;
use lastcall_engine::hunks::Hunk;
use lastcall_engine::roots::Badge;
use lastcall_engine::scan::{Annotation, Group, Pile, Row};
use lastcall_engine::store::RootKind;
use lastcall_engine::watcher::EngineEvent;

use super::input::{Action, DEFAULT_KEYMAP};

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

/// One root as the UI sees it: metadata plus the last pile, split into what the nav lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootView {
    pub meta: RootMeta,
    pub rows: Vec<Row>,
    pub groups: Vec<Group>,
    pub notices: Vec<String>,
}

impl RootView {
    pub fn new(meta: RootMeta) -> Self {
        Self {
            meta,
            rows: Vec::new(),
            groups: Vec::new(),
            notices: Vec::new(),
        }
    }

    /// Listed in the nav iff the pile has rows (kickoff ruling 2). An in-progress operation
    /// is a tag shown on a listed root, never a reason to list or unlist one.
    pub fn listed(&self) -> bool {
        !self.rows.is_empty()
    }

    pub fn row(&self, path: &[u8]) -> Option<&Row> {
        self.rows.iter().find(|r| r.path == path)
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Rescan every root (`Engine::scan_all` under the lock); `refresh_done` clears the flag.
    Refresh,
    /// Re-read `RootMeta::of` for every root and feed it to `sync_roots`.
    SyncRoots,
    Quit,
}

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
            keymap: DEFAULT_KEYMAP
                .iter()
                .map(|(name, specs)| {
                    (
                        (*name).to_owned(),
                        specs.iter().map(|s| (*s).to_owned()).collect(),
                    )
                })
                .collect(),
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
            for r in &view.rows {
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
            EngineEvent::Pile { root, pile, .. } => (self.apply_pile(root, pile), None),
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

    fn apply_pile(&mut self, root: PathBuf, pile: Pile) -> Changed {
        let Some(view) = self.roots.get_mut(&root) else {
            self.orphan_piles.insert(root, pile);
            return Changed::No;
        };
        if view.rows == pile.rows && view.notices == pile.notices {
            return Changed::No;
        }
        let fresh = pile
            .notices
            .iter()
            .find(|n| !view.notices.contains(n))
            .cloned();
        view.groups = pile.groups();
        view.rows = pile.rows;
        view.notices = pile.notices;
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
                        view.groups = pile.groups();
                        view.rows = pile.rows;
                        view.notices = pile.notices;
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
            changed = Changed::Yes;
        }
        if changed == Changed::Yes {
            self.reconcile_selection();
        }
        changed
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
        if self.help {
            self.help = false;
            return (Changed::Yes, None);
        }
        let changed = match target {
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
        EngineEvent::Pile {
            root: root(name),
            seq: 0,
            pile,
        }
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
        let mut p = pile("alpha");
        let mut extra = p.rows[0].hunks[0].clone();
        extra.index = 1;
        p.rows[0].hunks.push(extra);
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
