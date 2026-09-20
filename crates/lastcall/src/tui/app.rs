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
use std::time::{Duration, Instant, SystemTime};

use lastcall_engine::config::write::Setting;
use lastcall_engine::count::with_thousands;
use lastcall_engine::engine::{
    AcceptRequest, Accepted, EngineError, Flagged, RenderedHunk, RestoreRequest, Restored,
    RootState, Saved, Snoozed, Undone,
};
use lastcall_engine::git::{Mode, Oid};
use lastcall_engine::headstate::InProgress;
use lastcall_engine::hunks::{Expanded, Hunk, Tag};
use lastcall_engine::ledger::{FlagHunk, FlagSummary, LedgerError, iso8601_date, parse_iso8601};
use lastcall_engine::ops::{OpsError, Refused, Rendered};
use lastcall_engine::roots::Badge;
use lastcall_engine::scan::{Annotation, Change, Collapsed, Entry, Group, Pile, Row};
use lastcall_engine::store::{Current, RootKind};
use lastcall_engine::watcher::EngineEvent;

use super::herdr::{AgentCandidate, HerdrUpdate, HerdrView, Link, ToastRequest};
use super::input::{Action, EditKey, EditorKey, Keymap, NoteKey, PickKey, SnoozeKey, TourKey};
use super::render::key_label;
use super::textbuf::{TextBuf, Wrap};
use super::tour::{Card, EMPTY_CARD_MIN, Tour};
use super::wrap;
use unicode_width::UnicodeWidthStr;

/// The columns the bottom line keeps for the status text or the hints beside the notice
/// (design review F6): below this the notice takes a shorter form.
pub const NOTICE_MIN_TEXT: usize = 30;
pub const NAV_WIDTH_DEFAULT: u16 = 28;
pub const NAV_WIDTH_MIN: u16 = 16;
pub const NAV_WIDTH_MAX: u16 = 60;
/// Below this many columns the nav is hidden and the diff has focus.
pub const NAV_MIN_COLS: u16 = 70;
/// Below this the frame is the one-line "terminal too small" message.
pub const MIN_SIZE: (u16, u16) = (40, 10);
/// How far `PageUp` / `PageDown` move inside the note modal: its visible text height
/// (`render::NOTE_ROWS`), so a page is the page the reviewer can see.
pub const NOTE_PAGE: usize = super::render::NOTE_ROWS as usize;
/// The inline editor's line-number gutter: four columns of number and one of `▎`
/// (deliverable 8). The reducer needs it to clamp the horizontal scroll to the same text
/// width the renderer draws.
pub const EDITOR_GUTTER: usize = 5;

/// The per-root metadata the nav and the empty state show. Built from a `RootState` under
/// the engine lock (`RootMeta::of`), then owned by the app so rendering never locks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootMeta {
    pub path: PathBuf,
    /// What the nav calls this root: a repository's folder name, or a watched folder's
    /// name with as many parent folders as the configuration asks for.
    pub name: String,
    /// `path` as the header shows it, with the home folder written `~`.
    pub path_shown: String,
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
    /// `home` is the engine's canonicalized home (`Engine::home_shown`), the only thing
    /// `path_shown` needs that a `RootState` does not carry.
    pub fn of(root: &RootState, home: Option<&Path>) -> Self {
        Self {
            path: root.path.clone(),
            name: root.name(),
            path_shown: collapse_home(&root.path, home),
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
/// The launch hold (Gate 8 sponsor run ruling, spec §10 2026-09-07): from the first
/// `sync_roots` until every root has reported, nothing is listed and the right pane reads
/// `discovered N repos, checking status…`, so one repo is never shown as if it were the
/// only one with changes while the rest are still being scanned. After one second the pane
/// adds `K of N checked · F files pending so far · Ss` and a ✓ in the **leading column**
/// of each root that has reported (Design pass D4) — a single slow repo is then visible as
/// the one gap in that column. A
/// report is a `Scanned` tick, the root's pile, or a scan-failed notice; a global notice
/// (`watching …`) ends the hold outright, whatever has reported. The one thing that
/// outlives the scans is the herdr scope verdict: while `HerdrView::scope_pending` the
/// hold's frame continues (Design pass D5, ruling R7) with `scanned` set, and
/// `App::scope_settled` ends it — rather than the root list vanishing for one more
/// waiting screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loading {
    /// When the hold began, on the app clock (`App::now`).
    pub started: Instant,
    /// Roots that have reported → their pending rows.
    pub checked: BTreeMap<PathBuf, usize>,
    /// The scans are accounted for — every root reported, or a global notice said the
    /// rest never will — and the hold is only still on because the herdr scope verdict
    /// has not arrived (Design pass D5, ruling R7). The counter line then swaps
    /// `so far · Ss` for `waiting for herdr scope…`.
    pub scanned: bool,
}

impl Loading {
    /// How long a load runs before the counter line and the ✓s appear: below this a
    /// static line is all there is, so a fast launch shows one calm frame, not a flash of
    /// digits.
    pub const COUNTER_AFTER: Duration = Duration::from_secs(1);

    pub fn files(&self) -> usize {
        self.checked.values().sum()
    }

    /// Whether the counter line is shown at `now`.
    pub fn counting(&self, now: Instant) -> bool {
        now.duration_since(self.started) >= Self::COUNTER_AFTER
    }
}

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

    /// Whether the pile has pending rows. Since Amendment v1.9 this is no longer what puts
    /// a root on the nav — every repo in scope is listed — but what `hide_empty` and the
    /// scope notice's count mean by "empty". An in-progress operation is a tag shown on a
    /// listed root, never a reason to list or unlist one.
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

/// Where an entry sits **inside its own repo's** nav block, which `nav_entries` builds as
/// `[Root, rows by path, groups]`. Ordering by this key rather than by nav index is what
/// makes "the entry below" mean the same thing before and after the pile that removed the
/// selected one: a pile that added files sorting *above* the vanished row shifts every
/// index and none of these keys (design review F7).
fn nav_key(sel: &Selection) -> (u8, &[u8], u8) {
    match sel {
        Selection::Root(_) => (0, b"", 0),
        Selection::Row(_, path) => (1, path.as_slice(), 0),
        Selection::Group(_, kind) => (
            2,
            b"",
            match kind {
                Annotation::Upstream => 0,
                Annotation::Mixed => 1,
            },
        ),
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
    /// The `[U restore file]` hint on the main view's header line, beside `[A accept file]`.
    FileRestore,
    /// The `[u restore]` hint on hunk `i`'s header line, beside `[a accept]`.
    HunkRestore(usize),
    /// The `[m flag]` hint on hunk `i`'s header line, last in the run.
    HunkFlag(usize),
    /// The `[e expand]` control on a collapsed row's header line (Phase 6 deliverable 4);
    /// only drawn for `Glob`/`Size`, so a click can never reach a binary row.
    Expand,
    /// A root row's herdr dot: a click there acks the flag (deliverable 5).
    RootDot(PathBuf),
    /// The header's herdr badge: a click shows the full standalone reason.
    HeaderHerdr,
    /// The header's update notice (`↑ 0.1.1`): a click puts the whole sentence on the
    /// status line, exactly as a click on the badge does (Design pass D6).
    HeaderUpdate,
    /// Row `n` of the first-launch tour's card (Amendment v1.11): a choice row on a card
    /// that asks a question, and the footer on one that does not. A click takes it, exactly
    /// as Enter on it would — a card is a question, and a question answered by the mouse is
    /// still answered.
    TourRow(usize),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Changed {
    Yes,
    /// The default: a pass that folded nothing has nothing to draw.
    #[default]
    No,
}

impl From<bool> for Changed {
    fn from(yes: bool) -> Changed {
        if yes { Changed::Yes } else { Changed::No }
    }
}

impl Changed {
    pub fn or(self, other: Changed) -> Changed {
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
    /// Run `Engine::restore` for the one root it covers and feed the result to
    /// [`App::restored`]. A `Vec` for the same shape as `Accept`, though a restore has no
    /// group and no all variant — undoing a whole tree at once is not lastcall's gesture —
    /// so the vector never holds more than one request.
    Restore(Vec<(PathBuf, RestoreRequest)>),
    /// Write one flag through `Engine::flag`, off the UI task; the answer is
    /// `Local::Flagged`, which carries the paste-ready export.
    Flag {
        root: PathBuf,
        path: Vec<u8>,
        note: String,
        /// The hunk **as it was rendered** when `m` was pressed, with the total the export
        /// names. `None` flags the file.
        hunk: Option<RenderedHunk>,
        /// The row's shape when `m` was pressed, for a whole-file flag; `None` beside a
        /// `hunk` (Amendment v1.8).
        summary: Option<FlagSummary>,
        /// What the status line calls this flag (`f1`, `f1 hunk 2`). It travels with the
        /// write so its answer can name it without `App` holding a slot (F2).
        label: String,
    },
    /// Clear every flag on one path (`Engine::unflag`); the answer is `Local::Flagged` with
    /// an empty export.
    Unflag {
        root: PathBuf,
        path: Vec<u8>,
    },
    /// `pane.send_text` the export into one agent pane, bracketed-paste wrapped: it lands
    /// in the input box unsubmitted and the human presses Enter. The agent's own label is
    /// already on the optimistic status line; what travels is `flag`, the words a *failed*
    /// send has to name (F2).
    Stage {
        pane_id: String,
        flag: String,
        export: String,
    },
    /// No agent to stage to: append the export to this root's export file under the state
    /// dir. The one file the TUI writes, and the only writer of it.
    Export {
        root: PathBuf,
        /// The flag this export is of, for the answer's status line (F2).
        label: String,
        export: String,
    },
    /// Run `Engine::undo` for one root and feed the result to [`App::undone`]
    /// (Amendment v1.11). One root, never a list: `z` reverses the last accept in the
    /// repository the cursor is in, and an accept-all that spanned several roots left one
    /// entry on each of them.
    Undo(PathBuf),
    /// Run `Engine::snooze` for one root — `days` for a snooze, `None` to wake it — and
    /// feed the result to [`App::snoozed_result`]. The deadline is computed engine-side
    /// from its injected clock, so the TUI never reads a wall clock to build one.
    Snooze {
        root: PathBuf,
        days: Option<u32>,
    },
    /// The first-launch tour's one sanctioned config write (Amendment v1.11): set this key
    /// in the config file through `lastcall_engine::config::write` and feed the answer back
    /// to [`App::tour_written`]. The reducer has already applied the setting to the session
    /// by the time this is dispatched — the write is the *remembering*, not the doing — so a
    /// failure costs a footer, never the choice.
    TourWrite(Setting),
    /// The tour closed, by any path: write the marker so it is not shown again.
    TourDone,
    /// `agent.focus` on this pane id (herdr's own public id, never a name); the result
    /// comes back as `HerdrUpdate::Focused`.
    Focus(String),
    /// Tell the toast task which ready episodes opened and which ended (deliverable 6).
    Toast(ToastRequest),
    /// Suspend the TUI and open the user's `$EDITOR` on this path, at this line
    /// (deliverable 7; ruling P3). The loop owns the whole sequence — the CAS that proves
    /// the row is still what the user is looking at, the terminal handover, the child, and
    /// the resume — because every step of it is terminal or process state the reducer
    /// cannot see. `line` is [`Hunk::editor_line`] of the hunk under the cursor: the first
    /// line the agent actually changed, never the leading context above it (F9).
    EditExternal {
        root: PathBuf,
        /// The row as it was on screen. The loop refuses to open when the working tree no
        /// longer matches it, and the same value comes back as the `rendered` of the
        /// [`Effect::EditorReturned`] the resume raises, so the blessing question is asked
        /// against what the user saw.
        rendered: Rendered,
        line: usize,
    },
    /// The `$EDITOR` child exited: rehash this path off the UI task (`Engine::current`)
    /// and bring the answer back as [`Local::EditorReturned`](super::run::Local), which
    /// [`App::editor_returned`] folds (Phase 8 deliverable 3). The `rendered` row is the
    /// one the editor was opened on, so the comparison is against what the user saw and
    /// not against a pile the watcher may have applied while the editor had the terminal.
    EditorReturned {
        root: PathBuf,
        rendered: Rendered,
    },
    /// Read the live bytes of this row off the UI task (`Engine::read_rendered`) and bring
    /// them back as [`Local::EditRead`](super::run::Local), which opens the inline editor
    /// (deliverable 8). The whole opening context travels — the row, the line, the gutter
    /// marks and the band — because it is computed from the pile that was on screen when
    /// `i` was pressed, and a watcher pile that lands while the read is in flight must not
    /// move the marks under the file the user asked for.
    EditInline(EditOpen),
    /// Write the inline editor's buffer back through `Engine::save` (deliverable 1's CAS'd
    /// op) and bring the answer back as [`Local::Saved`](super::run::Local). `rendered` is
    /// the row the editor was opened on, so the save's compare-and-swap is against what the
    /// user saw — an agent that wrote meanwhile is a refusal, never an overwrite.
    Save {
        root: PathBuf,
        rendered: Rendered,
        bytes: Vec<u8>,
    },
    /// Put `bytes` on the user's clipboard with an OSC 52 write (deliverable 9). The
    /// payload is already capped ([`super::clipboard::CAP`]) and already the text the diff
    /// pane showed: the loop's only job is to encode it and hand it to the terminal.
    Copy(Vec<u8>),
    /// `Engine::hunks_of` for one collapsed row, off the UI task; the answer comes back as
    /// `Local::Expanded`. The **row** travels, not just its path: the expansion is computed
    /// from the oids the row was rendered from, so it shows exactly the delta the counts
    /// describe even if the file moved since (Phase 6 deliverable 4). Boxed because a `Row`
    /// is an order of magnitude wider than every other variant.
    Expand(PathBuf, Box<Row>),
}

/// One collapsed row's on-demand hunks, held beside the pile and never on the [`Row`]:
/// `accept_scope` and `apply_pile` do not read it, so `a`/`A` stay whole-row for a
/// collapsed path (§6.3 "single accept") and a newer pile cannot collapse the view
/// mid-read. Cleared when the selection leaves the row, and when a newer pile changes the
/// row's oids — which is why the oids it was computed from travel with it.
#[derive(Debug, Clone, PartialEq)]
pub struct Expansion {
    pub root: PathBuf,
    pub path: Vec<u8>,
    pub baseline: Option<Entry>,
    pub current: Option<Entry>,
    pub view: Expanded,
}

/// Everything the inline editor needs to open, captured when `i` was pressed
/// (deliverable 8).
///
/// It travels to the engine read and back so the editor is built from the pile the user was
/// looking at: the marks and the band are line numbers in *that* file, and a pile that
/// lands while the read is in flight cannot renumber them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOpen {
    pub root: PathBuf,
    pub rendered: Rendered,
    /// One-based line to put the caret on: [`Hunk::editor_line`] of the hunk under the
    /// cursor (F9), the same line `shift-i` hands `$EDITOR`.
    pub line: usize,
    /// Zero-based lines covered by **every** pending hunk of the row, so the gutter can
    /// show the reader where the rest of the agent's work is while they type.
    pub marks: Vec<usize>,
    /// Zero-based `(first, last)` of the hunk the editor opened at — the band. `None` for a
    /// row with no content hunk at all (a mode-only change opens at line 1 with no band).
    pub band: Option<(usize, usize)>,
    /// Which open this is: [`App::edit_gen`] at the moment `i` was pressed. It travels to
    /// the read and back so [`App::edit_read`] can tell the answer it is waiting for from
    /// an answer to an open that is no longer the live one (verifier (b) F2).
    pub generation: u64,
}

/// The inline editor, if open (deliverable 8; ruling P3).
///
/// It holds the **whole file** in a [`TextBuf`] and replaces the diff pane, so the reader
/// edits with the context around the hunk in front of them and the same mental model
/// `shift-i` gives them. The row it was opened on is kept verbatim: it is what the save's
/// compare-and-swap is against, and what the refusal messages name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    pub root: PathBuf,
    pub rendered: Rendered,
    pub buf: TextBuf,
    /// Zero-based lines inside a pending hunk, `▎` in the gutter. Kept in step with
    /// insertions and deletions above them, so the marks stay on the lines they describe as
    /// the reader types (they never become a second source of truth about the file — the
    /// next scan's hunks are).
    pub marks: Vec<usize>,
    /// The entered hunk's zero-based `(first, last)`, tinted as a band. It follows an edit
    /// above it like a mark, and **grows** with an edit inside it.
    pub band: Option<(usize, usize)>,
    /// The one-based line the editor opened at, for the `Esc, then i to reload` path and
    /// for the tests that prove the caret landed on the agent's first changed line.
    pub line_at_open: usize,
    /// A save is in flight: a second `Ctrl-S` is ignored until it answers, so one buffer
    /// cannot be written twice concurrently.
    pub saving: bool,
    /// The last save was refused: the header is red until the next key (the reader has to
    /// see that the file on disk is not what is in front of them).
    pub alarm: bool,
}

impl Editor {
    /// Whether line `i` (zero-based) is inside the band.
    pub fn in_band(&self, i: usize) -> bool {
        matches!(self.band, Some((a, b)) if i >= a && i <= b)
    }

    /// Whether line `i` (zero-based) carries a gutter mark.
    pub fn marked(&self, i: usize) -> bool {
        self.marks.binary_search(&i).is_ok()
    }

    /// Move the marks and the band after an edit that changed the line count by `delta` at
    /// line `at` (both zero-based).
    ///
    /// A mark strictly **below** the edit moves with it; a mark on the edited line stays,
    /// because the line the reader is typing on is the line the mark described. The band's
    /// end moves on `>=` rather than `>`, which is the whole difference between the two:
    /// splitting the band's last line leaves both halves inside the hunk the reader
    /// entered, so the tint has to grow with it.
    fn shift(&mut self, at: usize, delta: isize) {
        if delta == 0 {
            return;
        }
        let inside = self.marked(at);
        let moved = |x: usize| -> usize { x.saturating_add_signed(delta) };
        for m in &mut self.marks {
            if *m > at {
                *m = moved(*m);
            }
        }
        // Lines typed *into* a marked line are part of the change the mark describes, so
        // they are marked too — which is also what keeps the marks and the band agreeing
        // about a hunk the reader is typing inside.
        if inside && delta > 0 {
            self.marks.extend((at + 1)..=at + delta as usize);
        }
        self.marks.sort_unstable();
        self.marks.dedup();
        self.marks.retain(|m| *m < self.buf.line_count());
        if let Some((a, b)) = self.band {
            let a2 = if a > at { moved(a) } else { a };
            let b2 = if b >= at { moved(b) } else { b };
            self.band = (a2 <= b2).then_some((a2, b2.min(self.buf.line_count() - 1)));
        }
    }
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
    /// The post-`$EDITOR` blessing (deliverable 3, ruling P1): accept **the bytes the
    /// editor left on disk**, not the ones the row was rendered from.
    ///
    /// The only scope that carries its own [`Rendered`]. Every other variant is resolved
    /// against the held `RootView` at [`App::accept_requests`] time, which is exactly what
    /// a blessing must not do: the live oid was read by `Engine::current` after the editor
    /// exited, and the view still holds the pre-edit row (the watcher's pile for the save
    /// may not have arrived, and if it has, the row's oid is the same live one anyway).
    Bless {
        root: PathBuf,
        path: Vec<u8>,
        /// The pre-edit row with `oid`/`mode` replaced by what is on disk now.
        rendered: Box<Rendered>,
    },
}

/// What `accept` (`a`) answers with from the current selection: a scope it takes, or a
/// refusal that names the key which takes a whole entry.
///
/// Amendment v1.11, the maintainer's ruling of 2026-09-14: in the nav `accept_file` (`A`)
/// is the key that takes a whole entry — a file, a branch group, a repository — and `a`
/// takes a hunk and nothing larger. Before it, `a` on a repository row folded the whole
/// repository, and a repository of [`CONFIRM_ABOVE`] files or fewer vanished with no
/// question asked at all. An **empty** repository row has nothing to refuse over: `a` and
/// `A` both read [`NOTHING_TO_ACCEPT`] there, which [`App::request_accept`] says for either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptAnswer {
    /// `a` takes this much.
    Take(AcceptScope),
    /// `a` takes nothing here; this is the status text that says which key does.
    Refuse(String),
}

/// An accept the loop is running: its scope and the rows each request covered, so the
/// status can count what came back `Ok`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepting {
    pub scope: AcceptScope,
    pub files: Vec<(PathBuf, usize)>,
}

/// What one restore covers (§6.3). Deliberately fewer variants than [`AcceptScope`]: there
/// is no restore-group and no restore-all, so a restore is one hunk or one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreScope {
    /// Hunk `index` (0-based) of the `hunks` the row showed when the restore was asked.
    Hunk {
        root: PathBuf,
        path: Vec<u8>,
        index: usize,
        hunks: usize,
    },
    /// One row whole. `deleted` is a pending deletion going back on disk; `added` is a file
    /// that is not in the baseline at all, so putting it back **removes** it — which is why
    /// the two carry different confirm wording (kickoff deliverable 9, F16).
    File {
        root: PathBuf,
        path: Vec<u8>,
        deleted: bool,
        added: bool,
        hunks: usize,
    },
}

impl RestoreScope {
    pub fn root(&self) -> &Path {
        match self {
            RestoreScope::Hunk { root, .. } | RestoreScope::File { root, .. } => root,
        }
    }

    pub fn path(&self) -> &[u8] {
        match self {
            RestoreScope::Hunk { path, .. } | RestoreScope::File { path, .. } => path,
        }
    }
}

/// A restore the loop is running: the scope is all it takes to write the status line, since
/// a restore covers exactly one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restoring {
    pub scope: RestoreScope,
}

/// What a flag is being written against, **captured when `m` was pressed** and never
/// re-read afterwards: a pile landing while the note is open must not move the flag onto a
/// different hunk (kickoff F14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagTarget {
    Hunk {
        root: PathBuf,
        path: Vec<u8>,
        /// The row as it was on screen, kept so the engine seam takes the same tokens an
        /// accept or a restore would.
        rendered: Rendered,
        hunk: FlagHunk,
        /// **Content** hunks the row showed: the `of m` the export names.
        of: usize,
    },
    File {
        root: PathBuf,
        path: Vec<u8>,
        /// The row's shape as it was on screen: the export prints it instead of a diff
        /// (ruling P4, Amendment v1.8), and the modal's title says `whole file` because of
        /// it.
        ///
        /// `None` on a collapsed row that was never expanded: there is nothing the scan
        /// counted, and the export then prints no summary line (verifier (a) F2).
        summary: Option<FlagSummary>,
    },
}

impl FlagTarget {
    pub fn root(&self) -> &Path {
        match self {
            FlagTarget::Hunk { root, .. } | FlagTarget::File { root, .. } => root,
        }
    }

    pub fn path(&self) -> &[u8] {
        match self {
            FlagTarget::Hunk { path, .. } | FlagTarget::File { path, .. } => path,
        }
    }

    /// The modal's first line, and the words the status line uses for the flag afterwards:
    /// `f1 · hunk 2 of 3` or `f1 · whole file`.
    ///
    /// `whole file` and not `(file)`: it is the same phrase the export's header uses, so
    /// what the reviewer saw when they raised the flag and what the agent reads are one
    /// wording (ruling P4).
    pub fn label(&self) -> String {
        let lossy = String::from_utf8_lossy(self.path()).into_owned();
        match self {
            FlagTarget::Hunk { hunk, of, .. } => {
                format!("{lossy} · hunk {} of {of}", hunk.index + 1)
            }
            FlagTarget::File { .. } => format!("{lossy} · whole file"),
        }
    }

    /// The note modal's border title: ` flag hunk 2 of 3 ` / ` flag whole file `.
    ///
    /// The title names the target so the question "what am I about to flag?" is answered by
    /// the frame of the box, not only by a line inside it that a long path can crowd.
    pub fn modal_title(&self) -> String {
        match self {
            FlagTarget::Hunk { hunk, of, .. } => {
                format!(" flag hunk {} of {of} ", hunk.index + 1)
            }
            FlagTarget::File { .. } => " flag whole file ".to_owned(),
        }
    }

    /// The shape a whole-file flag covers; `None` for a hunk flag, which quotes its lines
    /// instead, and `None` for a collapsed row that was never expanded (F2).
    pub fn summary(&self) -> Option<FlagSummary> {
        match self {
            FlagTarget::Hunk { .. } => None,
            FlagTarget::File { summary, .. } => *summary,
        }
    }

    /// The shorter form the status line uses: `f1 hunk 2` / `f1`.
    pub fn status_label(&self) -> String {
        let lossy = String::from_utf8_lossy(self.path()).into_owned();
        match self {
            FlagTarget::Hunk { hunk, .. } => format!("{lossy} hunk {}", hunk.index + 1),
            FlagTarget::File { .. } => lossy,
        }
    }

    /// The rendered hunk the engine seam takes, `None` for a file flag.
    pub fn rendered_hunk(&self) -> Option<RenderedHunk> {
        match self {
            FlagTarget::Hunk { hunk, of, .. } => Some(RenderedHunk {
                hunk: hunk.clone(),
                of: *of,
            }),
            FlagTarget::File { .. } => None,
        }
    }
}

/// The note modal: what is being flagged, and the note being typed.
///
/// The note lives in the same [`TextBuf`] the inline editor uses (deliverable 5), so every
/// motion the buffer knows — word jumps, `Ctrl-A`/`Ctrl-E`, `Ctrl-K`, page keys — works
/// here without the modal implementing any of them, and a note longer than the box scrolls
/// through the buffer's own viewport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteEntry {
    pub target: FlagTarget,
    pub buf: TextBuf,
}

impl NoteEntry {
    /// The note as it will be written.
    pub fn text(&self) -> String {
        self.buf.text()
    }
}

/// The snooze modal (Amendment v1.11): which repository, and for how many days.
///
/// `days` is the digits as typed rather than a number, so backspacing to nothing shows an
/// empty field instead of jumping to `0`; [`SnoozeEntry::value`] is what Enter applies, and
/// it is the one place the 1..=365 range lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnoozeEntry {
    pub root: PathBuf,
    /// The repository's display name, captured when `s` was pressed: a pile landing while
    /// the modal is open must not retitle it.
    pub name: String,
    /// The digits typed so far. Seeded with [`SNOOZE_DEFAULT_DAYS`].
    pub days: String,
}

/// The snooze the modal offers before anything is typed (§6.7 as amended by v1.11).
pub const SNOOZE_DEFAULT_DAYS: u32 = 1;
/// The longest snooze the modal accepts. A year of not looking at a repository is already
/// further than anyone means; past it the digit is refused rather than clamped, so the
/// field never shows a number the write would not use.
pub const SNOOZE_MAX_DAYS: u32 = 365;
/// What `s` says when the selection is not a repository row.
pub const SNOOZE_NEEDS_ROOT: &str = "select a repository row to snooze it";
/// What `z` says when this root's undo stack is empty. The engine refuses with the same
/// words; this is the reducer's own path, for a root whose pile already says `undo: 0`.
pub const NOTHING_TO_UNDO: &str = "nothing to undo";
pub const UNDO_IN_PROGRESS: &str = "undo in progress";
pub const SNOOZE_IN_PROGRESS: &str = "snooze in progress";

impl SnoozeEntry {
    /// The number Enter applies: the digits as an integer, clamped into 1..=365, or the
    /// default when the field has been emptied.
    pub fn value(&self) -> u32 {
        match self.days.parse::<u32>() {
            Ok(0) | Err(_) => SNOOZE_DEFAULT_DAYS,
            Ok(n) => n.min(SNOOZE_MAX_DAYS),
        }
    }

    /// Type one digit, refusing anything that would take the field past
    /// [`SNOOZE_MAX_DAYS`] — so the field only ever shows a number the write would use.
    fn digit(&mut self, c: char) -> Changed {
        let mut next = self.days.clone();
        next.push(c);
        // A leading run of zeros is not a number anyone typed on purpose.
        let trimmed = next.trim_start_matches('0');
        match trimmed.parse::<u32>() {
            Ok(n) if (1..=SNOOZE_MAX_DAYS).contains(&n) => {
                self.days = trimmed.to_owned();
                Changed::Yes
            }
            _ => Changed::No,
        }
    }

    fn backspace(&mut self) -> Changed {
        match self.days.pop() {
            Some(_) => Changed::Yes,
            None => Changed::No,
        }
    }
}

/// The agent picker: which pane the export goes to when more than one is a candidate. The
/// flag is already on disk by the time this opens, so `Esc` loses nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    /// The root the flag was written against; the candidates are re-derived for it when
    /// herdr's news lands while the picker is open.
    pub root: PathBuf,
    /// The words the status line uses for the flag this picker is sending.
    pub label: String,
    pub export: String,
    pub candidates: Vec<AgentCandidate>,
    pub selected: usize,
}

/// Which of the two ledger writes a [`Local::Flagged`](super::run::Local) answers, and —
/// for a flag — the words its status line will use.
///
/// Carried in the message rather than read off a slot on `App` (verifier (b) F2). Two flag
/// writes can be in flight at once — `m` again while a send is out, or `m` then `shift-m`
/// on the same row, whose two blocking tasks the engine's mutex does not order — and a slot
/// that only says "a send is pending" mis-routes whichever answer arrives second: the
/// second flag was reported as `flags cleared` and never sent, and an unflag that overtook
/// its flag appended a blank entry to the day's export file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagKind {
    /// `m`: a note was written, and its export still has to reach an agent or the file.
    Flag { label: String },
    /// `shift-m`: the row's flags were cleared. Nothing to send.
    Unflag,
}

/// What the confirm modal is asking about. One modal, two operations: the title and the
/// first row come from the variant, and `confirm_counts` stays accept-only (F11) — a
/// restore covers one row, so there is nothing to tally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmScope {
    Accept(AcceptScope),
    Restore(RestoreScope),
    /// `Esc` on a dirty inline editor (deliverable 8): `y` throws the buffer away and
    /// closes, `n` goes back to it. The only scope that runs no engine op at all — what it
    /// guards is unwritten text, which lives nowhere but the buffer behind the modal.
    Discard {
        path: Vec<u8>,
    },
}

/// The confirm modal. Only the scope is stored: an accept's numbers are recomputed from the
/// held piles at every render ([`App::confirm_counts`]), so a pile applied underneath
/// changes them and `Confirm` folds exactly what is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub scope: ConfirmScope,
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
pub const RESTORE_IN_PROGRESS: &str = "restore in progress";
pub const NOTHING_TO_RESTORE: &str = "nothing to restore";
/// What `shift-i` says on a row there is no file to open (deliverable 7).
pub const NOT_EDITABLE: &str = "not editable";

/// The copy cue's text, and how long it stays up. Two seconds is long enough to read and
/// short enough that a reader who copied twice sees the second one arrive.
pub const COPIED: &str = "copied to clipboard";
pub const CUE_SECS: u64 = 2;

/// A selection the terminal would not take whole. It names the size because the only thing
/// the reader can do about it is select less, and they need to know by how much.
pub fn too_large_text(bytes: usize) -> String {
    format!(
        "selection too large to copy ({} KiB; the terminal would drop it)",
        bytes.div_ceil(1024)
    )
}
/// What the status says when the engine will not hand the inline editor the file's bytes
/// (deliverable 8): the reason, and the key that *can* open it anyway.
pub fn use_shift_i(why: &str) -> String {
    format!("use shift-i: {why}")
}
/// A save refused because the file moved under the editor: the buffer is still there, and
/// the two keys that get the reader out of it are spelled out (deliverable 8).
pub fn save_refused_text(path: &str) -> String {
    format!("{path}: changed since you opened it; not saved — Esc, then i to reload")
}

/// How long a status notice stays on the status line before the key hints return.
pub const STATUS_TTL: Duration = Duration::from_secs(30);

/// One root's accept result, as the loop hands it to the reducer.
pub type AcceptResult = Result<Accepted, AcceptFailed>;

/// One root's restore result. The failure classification is shared with accept: the two ops
/// fail for the same reasons (a busy ledger while the op takes the root, anything else by
/// message), and a second enum spelling the same two cases would only have to be kept in
/// step with the first.
pub type RestoreResult = Result<Restored, AcceptFailed>;

/// One root's flag (or unflag) result, on the same terms.
pub type FlagResult = Result<Flagged, AcceptFailed>;

/// One root's undo result (Amendment v1.11), classified like an accept's: an undo is a
/// ledger write on one root and fails for the same two reasons.
pub type UndoResult = Result<Undone, AcceptFailed>;

/// One root's snooze (or wake) result, on the same terms.
pub type SnoozeResult = Result<Snoozed, AcceptFailed>;

/// The inline editor's save result (deliverable 8), classified like an accept's: a save is
/// an op on one root's ledger and fails for the same two reasons.
pub type SaveResult = Result<Saved, AcceptFailed>;

/// Why one root's accept failed.
///
/// `LedgerBusy` is separate because it is a *whole-root* condition — another lastcall
/// process is mid-write on this root's ledger — where every `Refused` carries a row path and
/// renders against that row. It stays an `Err` off the accepted path so nothing is marked
/// seen, and the row it was asked for is still pending when the status line appears
/// (Phase 5 deliverable 2c).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptFailed {
    LedgerBusy,
    Other(String),
}

impl AcceptFailed {
    /// Classify what `Engine::accept` returned. Matched on the typed error, never on the
    /// message text.
    pub fn of(e: &EngineError) -> Self {
        match e {
            EngineError::Ops(OpsError::Ledger(LedgerError::LockBusy { .. })) => Self::LedgerBusy,
            other => Self::Other(other.to_string()),
        }
    }
}

/// A live line selection in the diff pane (deliverable 9): both ends are **absolute diff
/// line indices** into [`App::view_hunks`], and either may be the larger.
///
/// The anchor is where the selection started — the `v` keypress, or the mouse press — and
/// the cursor is where it has been dragged or scrolled to. Keeping them unordered is what
/// lets a reader select upwards and then back down through the anchor without the range
/// jumping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sel {
    pub anchor: usize,
    pub cursor: usize,
}

impl Sel {
    /// `(first, last)`, inclusive and in screen order.
    pub fn range(&self) -> (usize, usize) {
        (self.anchor.min(self.cursor), self.anchor.max(self.cursor))
    }
}

/// A short-lived message over the diff pane, independent of the status line (deliverable
/// 9). The status line is the record of what the *engine* did; a copy is a thing the
/// terminal did, and overwriting an accept's or a refusal's status with it would lose the
/// more important of the two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub text: String,
    /// Cleared by the first `Tick` at or after this instant.
    pub until: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub struct App {
    pub roots: BTreeMap<PathBuf, RootView>,
    pub selection: Option<Selection>,
    pub focus: Focus,
    pub diff: DiffCursor,
    /// Outer width of the nav pane (its right border is the divider), 16..=60.
    pub nav_width: u16,
    /// The nav's scroll offset in **nav lines** — the vector `render_nav` builds and
    /// windows, not `nav_entries()`, because separators, branch lines and the
    /// nothing-pending line are lines the reader scrolls past but can never select.
    ///
    /// Written back from `HitMap::nav_top` after each frame that drew the nav (deliverable
    /// 9), so scrolling survives the next pile: the reducers never touch it, and the
    /// clamp in `render_nav` handles a list that shrank underneath it.
    pub nav_top: usize,
    pub dragging: bool,
    /// The live line selection in the diff pane, if any (deliverable 9). Cleared by `Esc`,
    /// by a copy, and by anything that changes which row the diff is showing.
    pub sel: Option<Sel>,
    /// The diff line a **mouse press** landed on, while the button is still down: the
    /// anchor a `Drag` would extend from, and the answer to "where did this drag start?"
    /// that keeps a divider drag a divider drag (design review F13). `None` for a press
    /// anywhere else, which is why a drag over the nav selects nothing.
    pub press_line: Option<usize>,
    /// Whether a `Drag` has reached a different line since that press. A press and a
    /// release with nothing in between is a click, not a zero-length copy — including when
    /// the two arrive in one drained pass, because this is state and not a comparison of
    /// timestamps.
    pub drag_moved: bool,
    /// The copy cue, if one is up.
    pub cue: Option<Cue>,
    pub full_paths: bool,
    pub show_remote: bool,
    /// `t` (§6.7, Amendment v1.9 item 4): while `false` — the default, and what
    /// `hide_empty_repos` seeds — every repo the scope allows is on the nav, the ones with
    /// nothing pending as a name-and-branch row. While `true` those rows go, except a repo
    /// whose agent wants attention. Independent of the herdr scope (`w`).
    pub hide_empty: bool,
    /// `c` / `alt-z` (Phase 13, Amendment v1.14): visual word wrap in the diff pane. Seeded
    /// from `[ui] wrap`, whose default is `true` — a review tool that clips the end of a
    /// line asks the reader to accept text they have not read. Session state like
    /// [`App::hide_empty`]: the key flips it for this run and nothing is written to disk.
    pub wrap: bool,
    /// The diff body's size as the **last frame drew it** (columns, rows), copied out of
    /// `HitMap::diff_body` by [`crate::tui::run::Ui::rendered`] beside `nav_top`.
    ///
    /// The reducer's keep-visible arithmetic has to measure rows the way the renderer laid
    /// them out, and `page_rows()` is only an approximation of the body (the pane also
    /// draws the file header and sometimes notices above it). `None` before the first
    /// frame, after a `Resize` — up to `DRAIN_CAP` events fold before the next draw, so a
    /// size from before it would be the wrong one — and whenever the frame drew no hunks;
    /// [`App::diff_body_size`] then falls back to [`App::diff_cols`] and `page_rows`.
    pub diff_size: Option<(u16, u16)>,
    /// `shift-s` (Amendment v1.11): while `true` the snoozed repositories are listed too,
    /// each with a `snoozed until <date>` suffix on its branch line, and `s` on one of them
    /// wakes it. Session state like `hide_empty`, never written back.
    pub show_snoozed: bool,
    /// The nav index the current selection had when it was last **found** on the nav —
    /// written by `select` and refreshed by `reconcile_selection` whenever the selection
    /// survives a pile (design review F7). It is the only thing left to go on when the
    /// selection's whole repo has left the nav, so it is read exactly there:
    /// [`App::neighbour_after`]'s last rule.
    pub nav_anchor: Option<usize>,
    pub help: bool,
    pub status: Option<StatusLine>,
    /// Advanced by `Tick`; render computes ages from it, never from `Instant::now()`.
    pub now: Instant,
    /// The wall clock at the last `Tick`, as the **engine's** injected clock reports it
    /// (design review F4). The TUI has no clock of its own and never calls
    /// `SystemTime::now()`: the loop reads `engine.options().clock` once and hands the
    /// value down, so a `FixedClock` test and the snapshot tier decide what "now" is.
    ///
    /// `None` until the first tick, and in any test that does not set it — a snooze then
    /// simply never expires under the cursor, which is the safe direction: the next scan's
    /// pile carries the engine's own verdict.
    pub wall: Option<SystemTime>,
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
    /// Roots whose scan failed and so will never send a pile (`scan failed` notice); a
    /// later pile takes the root off it. With `seq`, this is what `pictured` reads.
    pub unscannable: std::collections::BTreeSet<PathBuf>,
    /// The accept the loop is running, if any; a second one is refused meanwhile.
    pub accepting: Option<Accepting>,
    /// The restore the loop is running, if any; a second one is refused meanwhile. Separate
    /// from `accepting` because the two write different things — the ledger and the working
    /// tree — and neither should silently stand in for the other in the status line.
    pub restoring: Option<Restoring>,
    /// The note modal, if open. While it is, every key edits the note except `ctrl-c`.
    pub note: Option<NoteEntry>,
    /// The snooze modal, if open (Amendment v1.11). While it is, every key goes to the day
    /// count except the non-printable quit spellings — the note modal's rule, for the same
    /// reason: a field that has the keyboard owns it.
    pub snooze: Option<SnoozeEntry>,
    /// The undo the loop is running, if any (the root it covers); a second `z` meanwhile is
    /// refused, exactly as a second accept is.
    pub undoing: Option<PathBuf>,
    /// The roots the last `ctrl-a` actually folded, when it covered more than one. It is
    /// what lets an undo's status line say how many other repositories are still holding an
    /// entry from that same fold; cleared by the next accept-all.
    pub accept_all_roots: Vec<PathBuf>,
    /// The snooze or wake the loop is running, if any (the root it covers).
    pub snoozing: Option<PathBuf>,
    /// The agent picker, if open (deliverable 10).
    pub picker: Option<Picker>,
    /// The inline editor, if open (deliverable 8). While it is, it replaces the diff pane
    /// and swallows every key the confirm modal above it does not take — `q` included, so
    /// a keymap letter types itself (F16).
    pub editor: Option<Editor>,
    /// The generation of the last [`Effect::EditInline`] issued, and whether its answer is
    /// still the one to honour (verifier (b) F2).
    ///
    /// The read runs off the UI task, so `i` pressed twice in one burst — key repeat, a
    /// pasted `ii`, a second press while a slow disk answers the first — used to produce
    /// two reads whose answers both opened an editor, the second one throwing away
    /// whatever the reader had typed into the first. `edit_pending` is `Some(gen)` from the
    /// press until its answer lands: a press while it is set asks for nothing, and an
    /// answer whose `generation` is not the pending one is dropped.
    pub edit_pending: Option<u64>,
    /// Monotonic counter behind [`App::edit_pending`]; every open gets its own number.
    pub edit_gen: u64,
    /// The confirm modal, if open: every action but `Tick`/`Resize`/`Confirm`/`Cancel`/
    /// `Quit` is ignored while it is (`Quit` passes as it does through the help overlay:
    /// `q` and ctrl-c quit by default, everywhere).
    pub confirm: Option<Confirm>,
    /// Everything herdr says, and the local ack episodes (Phase 5). Socket-free: the
    /// reducer never sees the client's own types.
    pub herdr: HerdrView,
    /// The first-launch welcome overlay, if open (Amendment v1.11). It sits above
    /// everything — the help overlay and the confirm modal included — and only its own keys
    /// reach the reducer while it is. Opened by the loop at the first frame past the launch
    /// hold and the scope verdict; see [`super::tour`].
    pub tour: Option<Tour>,
    /// The launch hold, until every root has reported. See [`Loading`].
    pub loading: Option<Loading>,
    /// The one collapsed row whose hunks `e` fetched, if any. A view, never a baseline;
    /// see [`Expansion`].
    pub expanded: Option<Expansion>,
    /// The effective key bindings, `(action name, key specs)` in `DEFAULT_KEYMAP` order.
    /// Seeded from the defaults; worker 3b replaces it after `Keymap::from_config` so the
    /// hint line and the help overlay show the user's own bindings.
    pub keymap: Vec<(String, Vec<String>)>,
    /// Whether this terminal reports the kitty keyboard protocol, asked once by
    /// [`super::term::enter`] and set by the loop before the first frame (ruling P9).
    ///
    /// It changes exactly two things: `Shift-Enter` is a newline in the note modal, and the
    /// modal's key line says so. `false` — the default, and what every terminal that does
    /// not answer gets — promises `Ctrl-J` alone, which always works.
    pub enhanced: bool,
    /// The newer release the once-a-day check found, if any (kickoff deliverable 2.6):
    /// the version alone (`0.1.1`), set once per session by `Local::UpdateAvailable` and
    /// never cleared. `None` is every other session, including every session the config
    /// turned the check off in.
    pub update: Option<String>,
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
            nav_top: 0,
            dragging: false,
            sel: None,
            press_line: None,
            drag_moved: false,
            cue: None,
            full_paths: false,
            show_remote: false,
            hide_empty: false,
            wrap: true,
            diff_size: None,
            show_snoozed: false,
            nav_anchor: None,
            help: false,
            status: None,
            now: Instant::now(),
            wall: None,
            size: (80, 24),
            refreshing: false,
            orphan_piles: BTreeMap::new(),
            seq: BTreeMap::new(),
            unscannable: std::collections::BTreeSet::new(),
            accepting: None,
            restoring: None,
            note: None,
            snooze: None,
            undoing: None,
            accept_all_roots: Vec::new(),
            snoozing: None,
            picker: None,
            editor: None,
            edit_pending: None,
            edit_gen: 0,
            confirm: None,
            herdr: HerdrView::default(),
            tour: None,
            loading: None,
            expanded: None,
            // The canonical spellings (`shift-a` shows as `A`), as `Keymap::table` gives.
            keymap: Keymap::defaults().table(),
            enhanced: false,
            update: None,
        }
    }

    /// The once-a-day check found `version`. Sets the header notice for the session and
    /// nothing else: no status sentence, no hint, no modal. The reader is mid-review and
    /// did not ask about releases (Design pass D6).
    pub fn update_available(&mut self, version: String) -> Changed {
        if self.update.as_deref() == Some(version.as_str()) {
            return Changed::No;
        }
        self.update = Some(version);
        Changed::Yes
    }

    /// The sentence [`Target::HeaderUpdate`] puts on the status line.
    pub fn update_sentence(version: &str) -> String {
        format!("lastcall {version} available — run: lastcall update")
    }

    /// The key specs bound to `action` (empty when unbound).
    pub fn keys_for(&self, action: &str) -> &[String] {
        match self.keymap.iter().find(|(n, _)| n == action) {
            Some((_, specs)) => specs.as_slice(),
            None => &[],
        }
    }

    // ---- derived views -------------------------------------------------------------------

    /// Whether `view` is listed in the nav. §6.7 as amended by v1.9 (ruling R2): **every**
    /// repo the active workspace scope allows is listed, once the launch hold and the scope
    /// verdict are past — a repo with nothing pending is a name-and-branch row, not an
    /// absence. `hide_empty` (`t`) is the only thing that takes one off, and even then an
    /// attention flag (a ready episode, acked or not, or a blocked agent) keeps it;
    /// `working`/`idle`/`unknown` annotate a root, they never decide one.
    pub fn is_listed(&self, view: &RootView) -> bool {
        let attention = || {
            self.herdr
                .flag(&view.meta.path)
                .is_some_and(|f| f.attention())
        };
        self.loading.is_none()
            && !self.herdr.scope_pending
            && self.herdr.in_scope(&view.meta.path)
            && (!self.hide_empty || view.listed() || attention())
            // Amendment v1.11: a snoozed repository is off the nav until its deadline
            // passes, `shift-s` shows it, or its agent wants attention — the `hide_empty`
            // exception exactly, and for the same reason: an agent that is blocked or done
            // is news the reader asked for before they asked for quiet.
            && (view.pile.snoozed_until.is_none() || self.show_snoozed || attention())
    }

    /// Whether `view` is snoozed as far as this frame is concerned.
    ///
    /// The pile's deadline is the engine's own verdict — it stamps `None` for a deadline
    /// that has already passed under its injected clock — and [`App::handle`]'s `Tick` arm
    /// drops one that expires while the TUI is open, so this is a field test and never a
    /// clock read (design review F4).
    pub fn snoozed<'a>(&self, view: &'a RootView) -> Option<&'a str> {
        view.pile.snoozed_until.as_deref()
    }

    /// Snoozed repositories the nav is not showing: the `N` of the `· N snoozed (S shows)`
    /// notice. Counted with [`Self::is_listed`]'s other rules in force, so the number
    /// promises exactly what `shift-s` will reveal.
    pub fn snoozed_out(&self) -> usize {
        if self.show_snoozed {
            return 0;
        }
        self.roots
            .values()
            .filter(|v| v.pile.snoozed_until.is_some())
            .filter(|v| {
                self.loading.is_none()
                    && !self.herdr.scope_pending
                    && self.herdr.in_scope(&v.meta.path)
                    && (!self.hide_empty
                        || v.listed()
                        || self.herdr.flag(&v.meta.path).is_some_and(|f| f.attention()))
                    && !self.herdr.flag(&v.meta.path).is_some_and(|f| f.attention())
            })
            .count()
    }

    /// The snooze half of the bottom-line notice, or `None` when nothing is snoozed away.
    /// The key comes from the keymap (`Keymap::table` canonicalises `shift-s` to `S`), so a
    /// rebound `show_snoozed` renames the notice with it.
    pub fn snooze_notice(&self) -> Option<String> {
        let n = self.snoozed_out();
        let key = self
            .keys_for("show_snoozed")
            .first()
            .cloned()
            .unwrap_or_default();
        (n > 0).then(|| format!("{n} snoozed ({key} shows)"))
    }

    /// The bottom line's right-hand notice at `width`, longest form that still leaves the
    /// status text or the hints room to read (design review F6).
    ///
    /// `render_status` has no width tiers and gains none here: the notice offers three
    /// forms and the line takes the first that fits.
    ///
    /// 1. `scope: alpha · 2 repos hidden (w shows all) · 1 snoozed (S shows)`
    /// 2. `scope: alpha · 2 hidden · 1 snoozed` — the parentheticals go; the keys are in
    ///    the help overlay, and the counts are the part that cannot be guessed.
    /// 3. `2 hidden · 1 snoozed` — the scope's label goes too.
    ///
    /// A form is taken while it leaves at least [`NOTICE_MIN_TEXT`] columns beside it;
    /// below that the shortest form stands and `render_status`'s own rule gives it the line
    /// alone rather than clipping the status text to nothing.
    pub fn bottom_notice(&self, width: u16) -> Option<String> {
        let forms = self.notice_forms();
        let first = forms.first()?;
        let width = width as usize;
        for form in &forms {
            if width.saturating_sub(form.width() + 2) >= NOTICE_MIN_TEXT {
                return Some(form.clone());
            }
        }
        Some(forms.last().unwrap_or(first).clone())
    }

    /// The three forms of [`Self::bottom_notice`], longest first. Empty when there is
    /// neither a scope nor a snooze to report.
    ///
    /// The ladder is the **combined** notice's rule (design review F6, whose two shortened
    /// examples are both combinations). A scope count on its own keeps the mandatory
    /// wording it has had since deliverable 8 — dropping its label would leave `2 hidden`
    /// with nothing to say what hid them, which is the trap that made the notice mandatory
    /// in the first place — and `render_status` gives it the line alone when the frame is
    /// too narrow for both, exactly as before.
    pub fn notice_forms(&self) -> Vec<String> {
        let scope = self.herdr.active_scope();
        let hidden = self.scoped_out();
        let snoozed = self.snoozed_out();
        if scope.is_none() && snoozed == 0 {
            return Vec::new();
        }
        let key = self
            .keys_for("show_snoozed")
            .first()
            .cloned()
            .unwrap_or_default();
        let join = |parts: Vec<String>| parts.join(" · ");
        let full = join(
            [
                scope.map(|s| {
                    format!(
                        "scope: {} · {} hidden (w shows all)",
                        s.label,
                        plural(hidden, "repo")
                    )
                }),
                (snoozed > 0).then(|| format!("{snoozed} snoozed ({key} shows)")),
            ]
            .into_iter()
            .flatten()
            .collect(),
        );
        let medium = join(
            [
                scope.map(|s| format!("scope: {} · {hidden} hidden", s.label)),
                (snoozed > 0).then(|| format!("{snoozed} snoozed")),
            ]
            .into_iter()
            .flatten()
            .collect(),
        );
        let short = join(
            [
                scope.map(|_| format!("{hidden} hidden")),
                (snoozed > 0).then(|| format!("{snoozed} snoozed")),
            ]
            .into_iter()
            .flatten()
            .collect(),
        );
        let mut forms = vec![full];
        if scope.is_some() && snoozed > 0 {
            for form in [medium, short] {
                if forms.last() != Some(&form) {
                    forms.push(form);
                }
            }
        }
        forms
    }

    /// Begin the launch hold over the roots `sync_roots` just installed (see [`Loading`]).
    /// With no roots there is nothing to wait for.
    pub fn start_loading(&mut self) {
        self.loading = (!self.roots.is_empty()).then(|| Loading {
            started: self.now,
            checked: BTreeMap::new(),
            scanned: false,
        });
    }

    /// One root has reported during the hold; the hold ends when every known root has.
    /// `Changed::Yes` while the hold is on (the pane counts), `No` once it is over.
    fn root_reported(&mut self, root: PathBuf, rows: usize) -> Changed {
        let Some(loading) = &mut self.loading else {
            return Changed::No;
        };
        loading.checked.insert(root, rows);
        if self.roots.keys().all(|r| loading.checked.contains_key(r)) {
            self.end_loading();
        }
        Changed::Yes
    }

    /// Every root's pile has landed, or its scan failed and none will. The launch hold
    /// ends on the last `Scanned` tick, which the scan pool sends the moment that root's
    /// scan finishes; the piles follow together once every scan is done, and the frame in
    /// between shows every root with nothing pending. The first-launch welcome counts the
    /// empty roots once, when it opens, so it waits for this and not only for the hold
    /// (Phase 10 fix worker's `14 of your 14` under load).
    pub fn pictured(&self) -> bool {
        self.roots
            .keys()
            .all(|r| self.seq.contains_key(r) || self.unscannable.contains(r))
    }

    /// The scans are accounted for. Design pass D5 / ruling R7: when the herdr scope
    /// verdict is still outstanding the hold's **frame** continues rather than being
    /// replaced by a second waiting screen, so the `Loading` value is kept (flagged
    /// `scanned`, which is what makes the counter line read `waiting for herdr scope…`)
    /// and `scope_settled` ends it. `is_listed` already gates on both, so nothing is
    /// listed a moment early either way.
    fn end_loading(&mut self) {
        let Some(loading) = &mut self.loading else {
            return;
        };
        if self.herdr.scope_pending {
            loading.scanned = true;
            return;
        }
        self.loading = None;
        self.reconcile_selection();
    }

    /// The scope verdict is in (any verdict — see `HerdrView::scope_pending`): list the
    /// roots. `Changed::Yes` only when something was being held back.
    pub fn scope_settled(&mut self) -> Changed {
        if !self.herdr.scope_pending {
            return Changed::No;
        }
        self.herdr.scope_pending = false;
        // The other half of D5 / R7: the hold's value was kept across the scope wait, and
        // with the verdict in the scans being accounted for is enough to end it. A hold
        // whose scans are still running ends the ordinary way, in `end_loading`.
        if self.loading.as_ref().is_some_and(|l| l.scanned) {
            self.loading = None;
        }
        self.reconcile_selection();
        Changed::Yes
    }

    /// Roots the active scope hides that would otherwise be listed: the `N` of the
    /// `scope: … · N repos hidden (w shows all)` notice.
    ///
    /// "Would otherwise be listed" is [`Self::is_listed`]'s own rule with the scope test
    /// removed, which since v1.9 includes an **empty** repo while `hide_empty` is off
    /// (verifier (a) F1): the notice promises what `w` will reveal, so counting by the
    /// pre-v1.9 "has rows or has a flag" left it short by every empty out-of-scope repo —
    /// `0 repos hidden` on a frame where `w` adds one.
    pub fn scoped_out(&self) -> usize {
        if self.herdr.active_scope().is_none() {
            return 0;
        }
        self.roots
            .values()
            .filter(|v| !self.herdr.in_scope(&v.meta.path))
            .filter(|v| {
                !self.hide_empty
                    || v.listed()
                    || self.herdr.flag(&v.meta.path).is_some_and(|f| f.attention())
            })
            .count()
    }

    /// The mandatory scope notice (deliverable 8), or `None` when no scope is in force.
    pub fn scope_notice(&self) -> Option<String> {
        let scope = self.herdr.active_scope()?;
        Some(format!(
            "scope: {} · {} hidden (w shows all)",
            scope.label,
            plural(self.scoped_out(), "repo")
        ))
    }

    /// Nav order: for each listed root by path, `[Root, rows…, groups…]`.
    pub fn nav_entries(&self) -> Vec<Selection> {
        let mut out = Vec::new();
        for (path, view) in &self.roots {
            if !self.is_listed(view) {
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
        self.roots.values().filter(|v| self.is_listed(v))
    }

    /// The selected row, if the selection is a row that still exists.
    pub fn selected_row(&self) -> Option<&Row> {
        match &self.selection {
            Some(Selection::Row(root, path)) => self.roots.get(root)?.row(path),
            _ => None,
        }
    }

    /// The expansion showing for the **selected** row, if any. `render_diff` and the diff
    /// cursor read it; `accept_scope` never does.
    pub fn expansion(&self) -> Option<&Expansion> {
        let exp = self.expanded.as_ref()?;
        match &self.selection {
            Some(Selection::Row(root, path)) if *root == exp.root && *path == exp.path => Some(exp),
            _ => None,
        }
    }

    /// The hunks the diff pane is showing for the selected row: the expansion's when one is
    /// held for it, else the row's own. Every diff-cursor bound goes through this, so `n`,
    /// `p` and the wheel work inside an expansion exactly as they do in a normal diff.
    /// **Accept does not**: `accept_scope` reads `row.hunks`, so a collapsed row stays a
    /// single accept however much of it is on screen (§6.3).
    pub fn view_hunks(&self) -> &[Hunk] {
        if let Some(exp) = self.expansion() {
            return &exp.view.hunks;
        }
        match self.selected_row() {
            Some(row) => &row.hunks,
            None => &[],
        }
    }

    /// Whether a row can be expanded at all: collapsed, and not by being binary.
    fn expandable(row: &Row) -> bool {
        matches!(row.collapsed, Some(Collapsed::Glob) | Some(Collapsed::Size))
    }

    /// `e`: ask the loop for the selected collapsed row's hunks. A no-op — no effect and no
    /// draw — on a binary row, on a row that is not collapsed, and on a row already
    /// expanded, so the key is silent exactly where it has nothing to do.
    fn request_expand(&mut self) -> (Changed, Option<Effect>) {
        let Some(Selection::Row(root, path)) = self.selection.clone() else {
            return (Changed::No, None);
        };
        let Some(row) = self.selected_row() else {
            return (Changed::No, None);
        };
        if !Self::expandable(row) {
            return (Changed::No, None);
        }
        if self
            .expanded
            .as_ref()
            .is_some_and(|e| e.root == root && e.path == path)
        {
            return (Changed::No, None);
        }
        (
            Changed::No,
            Some(Effect::Expand(root, Box::new(row.clone()))),
        )
    }

    /// An `Effect::Expand` came back for `asked`, the row `hunks_of` was given. It is
    /// dropped unless that row is still selected **and** still carries the oids the answer
    /// was computed from: a pile that landed between the request and the answer makes it a
    /// diff of something the screen is no longer showing, and storing it under the row's
    /// new oids would keep it there for as long as those oids last (verifier (b) F1).
    pub fn set_expanded(&mut self, root: PathBuf, asked: &Row, view: Expanded) -> Changed {
        let path = asked.path.clone();
        let matches_selection = matches!(
            &self.selection,
            Some(Selection::Row(r, p)) if *r == root && *p == path
        );
        let same_oids = self
            .roots
            .get(&root)
            .and_then(|v| v.row(&path))
            .is_some_and(|row| row.baseline == asked.baseline && row.current == asked.current);
        if !matches_selection || !same_oids {
            return Changed::No;
        }
        let next = Expansion {
            root,
            path,
            baseline: asked.baseline.clone(),
            current: asked.current.clone(),
            view,
        };
        if self.expanded.as_ref() == Some(&next) {
            return Changed::No;
        }
        self.expanded = Some(next);
        self.clamp_cursor();
        Changed::Yes
    }

    /// Drop the expansion when it no longer describes what is on screen: the selection left
    /// the row, the row is gone, or a newer pile changed its oids.
    fn drop_stale_expansion(&mut self) {
        let Some(exp) = &self.expanded else {
            return;
        };
        let selected = matches!(
            &self.selection,
            Some(Selection::Row(r, p)) if *r == exp.root && *p == exp.path
        );
        let same_oids = self
            .roots
            .get(&exp.root)
            .and_then(|v| v.row(&exp.path))
            .is_some_and(|row| row.baseline == exp.baseline && row.current == exp.current);
        if !selected || !same_oids {
            self.expanded = None;
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
            EngineEvent::Pile { root, seq, pile } => {
                let rows = pile.rows.len();
                let changed = self.apply_pile(root.clone(), seq, pile);
                (changed.or(self.root_reported(root, rows)), None)
            }
            EngineEvent::Scanned { root, rows } => (self.root_reported(root, rows), None),
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
                    self.unscannable.remove(root);
                }
                if result == Changed::Yes {
                    self.reconcile_selection();
                }
                let effect = (!changed.added.is_empty() || !changed.removed.is_empty())
                    .then_some(Effect::SyncRoots);
                (result, effect)
            }
            EngineEvent::Notice { root, text } => {
                match &root {
                    // A failed scan is still that root's report.
                    Some(r) if text.starts_with("scan failed") => {
                        self.unscannable.insert(r.clone());
                        self.root_reported(r.clone(), 0);
                    }
                    // Every global notice comes after the initial scans (`watching …`,
                    // `watch installation failed`): whatever has not reported never will.
                    None => self.end_loading(),
                    Some(_) => {}
                }
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
        self.unscannable.remove(&root);
        let Some(view) = self.roots.get_mut(&root) else {
            self.orphan_piles.insert(root, pile);
            return Changed::No;
        };
        if view.pile == pile {
            return Changed::No;
        }
        tracing::debug!(
            root = %view.meta.name,
            seq,
            rows = pile.rows.len(),
            was = view.pile.rows.len(),
            "pile"
        );
        let fresh = pile
            .notices
            .iter()
            .find(|n| !view.pile.notices.contains(n))
            .cloned();
        view.set_pile(pile);
        if let Some(n) = fresh {
            self.set_status(n);
        }
        self.drop_stale_expansion();
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
            self.unscannable.remove(&k);
            changed = Changed::Yes;
        }
        if changed == Changed::Yes {
            self.reconcile_selection();
        }
        changed
    }

    // ---- accepting -----------------------------------------------------------------------

    /// What `Accept` covers from here: on a file row **with hunks**, the one hunk under the
    /// diff cursor — whichever pane has focus, so `a` from the nav takes a hunk, not the
    /// file (`A` / `accept_file` is the only key that takes a whole file). A file row with
    /// no hunks (binary, collapsed, deleted, unreadable) has no hunk to point at, so `a`
    /// there keeps taking the row whole.
    ///
    /// A **group** entry and a **non-empty repository** row are refusals (Amendment v1.11,
    /// the ruling of 2026-09-14): they are several files, and `A` is the key that takes
    /// several. The refusal names the `accept_file` key from the effective keymap, so a
    /// reader who rebound it is sent to their own key. `None` with nothing selected or a
    /// vanished row.
    pub fn accept_scope(&self) -> Option<AcceptAnswer> {
        match self.selection.clone()? {
            Selection::Row(root, path) => {
                let row = self.roots.get(&root)?.row(&path)?;
                if !row.hunks.is_empty() {
                    Some(AcceptAnswer::Take(AcceptScope::Hunk {
                        root,
                        path,
                        index: self.diff.hunk.min(row.hunks.len() - 1),
                        hunks: row.hunks.len(),
                    }))
                } else {
                    Some(AcceptAnswer::Take(file_scope(root, row)))
                }
            }
            Selection::Group(..) => Some(AcceptAnswer::Refuse(format!(
                "{} accepts the group",
                self.accept_file_key()
            ))),
            // An empty repository row keeps the old answer: `request_accept` counts nothing
            // and says `nothing to accept`, which is what `A` says there too.
            Selection::Root(root) if self.rows_in(&root) == 0 => {
                Some(AcceptAnswer::Take(AcceptScope::Root(root)))
            }
            Selection::Root(root) => Some(AcceptAnswer::Refuse(format!(
                "{} accepts all in {}",
                self.accept_file_key(),
                self.root_name(&root)
            ))),
        }
    }

    /// The `accept_file` key as the help overlay spells it, from the **effective** keymap.
    ///
    /// `[keys]` cannot leave an action unbound (an empty list is a config error and every
    /// default action is in the table), so the fallback is unreachable; the default
    /// spelling is the honest thing to print if it ever is not.
    fn accept_file_key(&self) -> String {
        self.keys_for("accept_file")
            .first()
            .map(|s| key_label(s))
            .unwrap_or_else(|| "A".to_owned())
    }

    /// How many rows a root holds now, `0` for one this app has never seen.
    fn rows_in(&self, root: &Path) -> usize {
        self.roots.get(root).map_or(0, |v| v.rows().len())
    }

    /// What `AcceptFile` covers: the whole selected entry, whichever pane has focus — a
    /// file row, a branch group, or every row of a repository from its row (Amendment
    /// v1.11). Above [`CONFIRM_ABOVE`] files it asks first, exactly as `^A` does.
    pub fn accept_file_scope(&self) -> Option<AcceptScope> {
        match self.selection.clone()? {
            Selection::Row(root, path) => {
                let row = self.roots.get(&root)?.row(&path)?;
                Some(file_scope(root, row))
            }
            Selection::Group(root, kind) => Some(AcceptScope::Group { root, kind }),
            Selection::Root(root) => Some(AcceptScope::Root(root)),
        }
    }

    /// What `Restore` (`u`) covers: the hunk under the diff cursor on a row that has
    /// content hunks, else the selected row whole. A **deletion** row's single hunk is the
    /// file (F16), so `u` on it is a file restore and asks like `shift-u` does. Groups and
    /// root entries have no restore: there is no restore-group and no restore-all.
    ///
    /// An **added** file whose whole content is one hunk is the same case seen from the
    /// other side (verifier (b) F3): restoring that hunk is not "put a hunk back", it is
    /// `restore_file`'s removal path — the engine's own
    /// `ops_restore_hunk_on_an_added_file_removes_it` says so. Routing it to the hunk scope
    /// deleted the file with no confirm at all and then reported `restored f1 hunk 1` for a
    /// file that no longer existed. Kickoff ruling item 2 says a restore that *deletes* an
    /// added file asks, so the whole-row scope is the honest one: the confirm reads
    /// `Delete <path>? (added since baseline)` and the status line `removed <path> (added
    /// since baseline)` (decision (l)). The mode hunk is not content, exactly as
    /// `flag_target` counts it; an added row carrying more than one content hunk (an
    /// expansion) keeps the hunk scope, because there a hunk restore really is partial.
    pub fn restore_scope(&self) -> Option<RestoreScope> {
        match self.selection.clone()? {
            Selection::Row(root, path) => {
                let row = self.roots.get(&root)?.row(&path)?;
                let content = row.hunks.iter().filter(|h| !h.is_mode_change()).count();
                if row.change == Change::Added && content == 1 {
                    return Some(restore_file_of(root, row));
                }
                if row.change != Change::Deleted && !row.hunks.is_empty() {
                    Some(RestoreScope::Hunk {
                        root,
                        path,
                        index: self.diff.hunk.min(row.hunks.len() - 1),
                        hunks: row.hunks.len(),
                    })
                } else {
                    Some(restore_file_of(root, row))
                }
            }
            _ => None,
        }
    }

    /// What `RestoreFile` (`shift-u`) covers: the selected row whole, whichever pane has
    /// focus. Never a group and never a root, for the same reason.
    pub fn restore_file_scope(&self) -> Option<RestoreScope> {
        match self.selection.clone()? {
            Selection::Row(root, path) => {
                let row = self.roots.get(&root)?.row(&path)?;
                Some(restore_file_of(root, row))
            }
            _ => None,
        }
    }

    /// The one request a restore scope means right now, built from the held `RootView` and
    /// nothing else. Empty when the scope no longer covers a row.
    pub fn restore_requests(&self, scope: &RestoreScope) -> Vec<(PathBuf, RestoreRequest)> {
        let mut out = Vec::new();
        match scope {
            RestoreScope::Hunk {
                root, path, index, ..
            } => {
                if let Some(row) = self.roots.get(root).and_then(|v| v.row(path))
                    && *index < row.hunks.len()
                {
                    out.push((
                        root.clone(),
                        RestoreRequest::Hunk {
                            rendered: Rendered::of(row),
                            hunks: row.hunks.clone(),
                            index: *index,
                        },
                    ));
                }
            }
            RestoreScope::File { root, path, .. } => {
                if let Some(row) = self.roots.get(root).and_then(|v| v.row(path)) {
                    out.push((root.clone(), RestoreRequest::File(Rendered::of(row))));
                }
            }
        }
        out
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
                    // R5: the branch the pile these rows came from was scanned under, so
                    // the engine can refuse the write when the record in force has moved.
                    let rendered_on = self
                        .roots
                        .get(root)
                        .and_then(|v| v.pile.seen_branch.clone());
                    out.push((
                        root.clone(),
                        AcceptRequest::Group {
                            rows: rendered,
                            rendered_on,
                        },
                    ));
                }
            }
            AcceptScope::Root(root) => {
                if let Some(view) = self.roots.get(root).filter(|v| v.listed()) {
                    out.push((root.clone(), AcceptRequest::All(view.pile.clone())));
                }
            }
            AcceptScope::Bless { root, rendered, .. } => {
                out.push((root.clone(), AcceptRequest::File((**rendered).clone())));
            }
            AcceptScope::All => {
                // "Every listed root" is the nav's own rule (deliverable 8): a root the
                // active scope hides is not on screen, so accept-all never touches it.
                for (root, view) in &self.roots {
                    if view.listed() && self.is_listed(view) {
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
            AcceptScope::Hunk { root, path, .. }
            | AcceptScope::File { root, path, .. }
            | AcceptScope::Bless { root, path, .. } => {
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
                // The same rule as `accept_requests`, so the confirm modal's numbers and
                // names describe exactly the roots the accept will cover.
                for (root, view) in &self.roots {
                    if self.is_listed(view) {
                        tally(root, &view.rows().iter().collect::<Vec<_>>());
                    }
                }
            }
        }
        counts
    }

    /// The confirm modal's numbers, from the held piles as they are now.
    pub fn confirm_counts(&self) -> Option<ConfirmCounts> {
        match self.confirm.as_ref()?.scope {
            ConfirmScope::Accept(ref scope) => Some(self.counts_of(scope)),
            // A restore covers one row: there is nothing to tally, and the modal's rows
            // come from the scope itself (F11).
            ConfirmScope::Restore(_) | ConfirmScope::Discard { .. } => None,
        }
    }

    /// The restore the confirm modal is asking about, if it is asking about one.
    pub fn confirm_restore(&self) -> Option<&RestoreScope> {
        match self.confirm.as_ref()?.scope {
            ConfirmScope::Restore(ref scope) => Some(scope),
            ConfirmScope::Accept(_) | ConfirmScope::Discard { .. } => None,
        }
    }

    /// The buffer the confirm modal is asking to throw away, if it is asking that
    /// (deliverable 8), on the same terms as [`App::confirm_restore`].
    pub fn confirm_discard(&self) -> Option<&[u8]> {
        match self.confirm.as_ref()?.scope {
            ConfirmScope::Discard { ref path } => Some(path),
            _ => None,
        }
    }

    /// The blessing the confirm modal is asking about, if it is asking about one: the path
    /// the `$EDITOR` session changed. Kept beside [`App::confirm_restore`] so `render_confirm`
    /// asks one question per shape and never matches the scope enum itself.
    pub fn confirm_bless(&self) -> Option<&[u8]> {
        match self.confirm.as_ref()?.scope {
            ConfirmScope::Accept(AcceptScope::Bless { ref path, .. }) => Some(path),
            _ => None,
        }
    }

    /// The `$EDITOR` child exited and the path was rehashed: decide what the session meant
    /// (deliverable 3; ruling P1 = Amendment v1.8).
    ///
    /// lastcall cannot tell who wrote the bytes — the user's editor, or an agent that wrote
    /// while the editor had the terminal — so a *changed* file is never blessed silently:
    /// it opens the confirm, and the user, who knows whether they saved, answers it. That
    /// confirm is what makes the blessing an admissible exception to invariant 3 ("accept is
    /// metadata-only and never a fresh read"): the answer, not the read, is the review.
    ///
    /// Everything else writes nothing:
    /// - **unchanged** (the same oid *and* mode) — the usual outcome of a look-and-quit, and
    ///   of every non-waiting editor (`code` without `--wait`), whose later save arrives as
    ///   an ordinary pending row;
    /// - **gone**, **a symlink**, or **unhashable** on return — there is no content to
    ///   bless, and the row stays pending with the reason on the status line;
    /// - **another modal is already open** (a confirm, the note, the agent picker) — the
    ///   question would replace one the user is reading, so it is not asked at all.
    pub fn editor_returned(
        &mut self,
        root: PathBuf,
        rendered: Rendered,
        live: Current,
    ) -> (Changed, Option<Effect>) {
        let path = String::from_utf8_lossy(&rendered.path).into_owned();
        let (oid, mode) = match live {
            Current::Absent => {
                self.set_status(format!("{path}: deleted on return; left pending"));
                return (Changed::Yes, None);
            }
            // `hash_path` phrases these: `not a regular file` for a fifo or socket,
            // `typechange: a directory where a file was`, an `EACCES` message. Each is a
            // reason a reader can act on, so it is passed through rather than flattened.
            Current::Unhashable(why) => {
                self.set_status(format!("{path}: {why} on return; left pending"));
                return (Changed::Yes, None);
            }
            // A symlink hashes fine — it is its target's bytes — but it is not a file the
            // editor edited in place, so it takes the same sentence a fifo does.
            Current::Present {
                mode: Mode::Symlink,
                ..
            } => {
                self.set_status(format!(
                    "{path}: not a regular file on return; left pending"
                ));
                return (Changed::Yes, None);
            }
            Current::Present { oid, mode } => (oid, mode),
        };
        if rendered.oid.as_ref() == Some(&oid) && rendered.mode == Some(mode) {
            self.set_status("no change");
            return (Changed::Yes, None);
        }
        // Another question is already on screen. The return arrives on a channel, so a
        // confirm the user opened between the resume and the reply (or a note, or the agent
        // picker) would be silently replaced and the next `y` would answer *this* question
        // instead of the one they read — verifier (a) F1. The row keeps the editor's delta,
        // so the edit is not lost: it is reviewed as an ordinary pending row.
        if self.confirm.is_some() || self.note.is_some() || self.picker.is_some() {
            self.set_status(format!("{path}: changed on return; left pending"));
            return (Changed::Yes, None);
        }
        let live_rendered = Rendered {
            oid: Some(oid),
            mode: Some(mode),
            ..rendered
        };
        self.confirm = Some(Confirm {
            scope: ConfirmScope::Accept(AcceptScope::Bless {
                root,
                path: live_rendered.path.clone(),
                rendered: Box::new(live_rendered),
            }),
        });
        (Changed::Yes, None)
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
            self.confirm = Some(Confirm {
                scope: ConfirmScope::Accept(scope),
            });
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
                    AcceptRequest::Group { rows, .. } => rows.len(),
                    AcceptRequest::All(pile) => pile.rows.len(),
                };
                (root.clone(), n)
            })
            .collect();
        self.accepting = Some(Accepting { scope, files });
        self.set_status("accepting…");
        (Changed::Yes, Some(Effect::Accept(reqs)))
    }

    /// What `m` flags, captured from the row on screen **now**: the hunk under the diff
    /// cursor when the diff has focus and there is a content hunk there, else the file. A
    /// deletion row's single hunk is not a hunk to discuss, so the nav's answer and the
    /// diff's agree there: the file.
    ///
    /// The hunks it reads are the ones **on screen** ([`App::view_hunks`]) — a collapsed
    /// row's expansion when `e` opened one, else the row's own. Accept and restore stay
    /// whole-row on a collapsed row (§6.3, and the expansion's line cap), but a flag only
    /// quotes: `m` on hunk 2 of 3 of an expansion is about that hunk, and the export says
    /// `hunk 2 of 3` (verifier (b) F5).
    pub fn flag_target(&self) -> Option<FlagTarget> {
        let Selection::Row(root, path) = self.selection.clone()? else {
            return None;
        };
        let row = self.roots.get(&root)?.row(&path)?;
        let hunks = self.view_hunks();
        let content = hunks.iter().filter(|h| !h.is_mode_change()).count();
        if self.effective_focus() == Focus::Diff && row.change != Change::Deleted && content > 0 {
            let index = self.diff.hunk.min(hunks.len() - 1);
            let hunk = &hunks[index];
            // The mode hunk is not content: there is nothing to quote, so `m` on it flags
            // the file (the same rule the export's `of` count follows).
            if !hunk.is_mode_change() {
                return Some(FlagTarget::Hunk {
                    root,
                    path: path.clone(),
                    rendered: Rendered::of(row),
                    hunk: FlagHunk {
                        index,
                        header: hunk_header(hunk),
                        text: hunk_body(hunk),
                    },
                    of: content,
                });
            }
        }
        // The shape travels with a whole-file flag because the export has no diff to show
        // (ruling P4): the counts are the row's as rendered, taken now, for the same reason
        // `of` is (F14).
        //
        // Except on a collapsed row nobody expanded: it has no hunks to count, and on a
        // Binary row the `+a −d` are not line counts of anything the scan diffed. A summary
        // there would tell the agent a changed file has zero hunks, which is a claim about
        // the file rather than about lastcall's view of it — so there is no summary and the
        // export prints no summary line (verifier (a) F2).
        let counted = row.collapsed.is_none() || self.expansion().is_some();
        Some(FlagTarget::File {
            root,
            path,
            summary: counted.then_some(FlagSummary {
                hunks: content,
                added: row.added,
                deleted: row.deleted,
            }),
        })
    }

    /// What `shift-i` (and, in deliverable 8, `i`) would open: the selected row and the
    /// **one-based line** an editor should land on inside it.
    ///
    /// The row is the one the user is looking at and the line is the one they are looking
    /// at inside it: with the diff focused and a content hunk under the cursor, that hunk's
    /// [`Hunk::editor_line`] — the first line the agent actually changed, never
    /// `new_range.start + 1`, which is three lines of leading context above it (design
    /// review F9). From the nav, and from a diff cursor parked on the synthetic mode hunk
    /// (there is no text in it to open at), the row's **first content** hunk; a row with no
    /// content hunk at all — a collapsed file nobody expanded, a binary row — opens at
    /// line 1, which is the honest answer for "somewhere in this file".
    ///
    /// `None` on the three rows there is no file to open: a **deletion** (the path is gone
    /// — restoring it is `u`, not an editor), a **symlink** (opening it would edit its
    /// target, which is a different file from the one the row is about) and a path whose
    /// bytes are **not UTF-8** (the argv handed to the editor is built from it, and a byte
    /// string that is not text is not something to hand a process blind). The caller says
    /// [`NOT_EDITABLE`]; deliverable 8's own refusals are the engine's, and say more.
    pub fn edit_target(&self) -> Option<(PathBuf, Rendered, usize)> {
        let Selection::Row(root, path) = self.selection.clone()? else {
            return None;
        };
        let row = self.roots.get(&root)?.row(&path)?;
        if row.change == Change::Deleted {
            return None;
        }
        if !matches!(
            row.current.as_ref().map(|e| e.mode),
            Some(Mode::Regular) | Some(Mode::Executable)
        ) {
            return None;
        }
        if std::str::from_utf8(&path).is_err() {
            return None;
        }
        let line = self.edit_hunk().map_or(1, |h| h.editor_line());
        Some((root, Rendered::of(row), line))
    }

    /// The content hunk an editor key opens at: the one under the diff cursor when the diff
    /// has focus, else the row's first — the choice deliverable 7 made for the `$EDITOR`
    /// line, shared with deliverable 8 so `i` and `shift-i` can never land on two different
    /// hunks of one row. The synthetic mode hunk is never it: there is no text in it.
    fn edit_hunk(&self) -> Option<&Hunk> {
        let hunks = self.view_hunks();
        let under_cursor = (self.effective_focus() == Focus::Diff)
            .then(|| hunks.get(self.diff.hunk.min(hunks.len().saturating_sub(1))))
            .flatten()
            .filter(|h| !h.is_mode_change());
        under_cursor.or_else(|| hunks.iter().find(|h| !h.is_mode_change()))
    }

    /// `i`: ask the loop for the row's live bytes, so the inline editor can open on them.
    ///
    /// Nothing is drawn on the way out — the read is one `lstat` and one file read off the
    /// UI task — and the marks and the band are computed **here**, from the pile that is on
    /// screen, so the editor that opens describes the file the reader was looking at.
    ///
    /// One open at a time (verifier (b) F2). An editor that is already up swallows `i` as
    /// text, so the first arm is defence for a caller that is not the keymap; the second is
    /// the one that fires — a second press while the first read is still in flight asks for
    /// nothing, because the editor it would open is the editor about to open. Neither says
    /// anything: the reader is looking at the buffer they asked for either way.
    fn edit_inline(&mut self) -> (Changed, Option<Effect>) {
        if !matches!(self.selection, Some(Selection::Row(..))) {
            return (Changed::No, None);
        }
        if self.editor.is_some() || self.edit_pending.is_some() {
            return (Changed::No, None);
        }
        let Some((root, rendered, line)) = self.edit_target() else {
            self.set_status(NOT_EDITABLE);
            return (Changed::Yes, None);
        };
        let mut marks: Vec<usize> = self
            .view_hunks()
            .iter()
            .filter(|h| !h.is_mode_change())
            .flat_map(|h| h.new_range.clone())
            .collect();
        marks.sort_unstable();
        marks.dedup();
        let band = self
            .edit_hunk()
            .filter(|h| !h.new_range.is_empty())
            .map(|h| (h.new_range.start, h.new_range.end - 1));
        self.edit_gen += 1;
        self.edit_pending = Some(self.edit_gen);
        (
            Changed::No,
            Some(Effect::EditInline(EditOpen {
                root,
                rendered,
                line,
                marks,
                band,
                generation: self.edit_gen,
            })),
        )
    }

    /// The engine answered [`Effect::EditInline`]: open the editor on the bytes, or say why
    /// this file will not go in a buffer (deliverable 8).
    ///
    /// A [`Refused::NotEditable`] prints its `why` on its own and names the key that opens
    /// the file anyway — `use shift-i: binary` — because "no" is only half an answer when
    /// there is a second way in. Every other refusal is the CAS speaking, and says so in
    /// the vocabulary every other refused op uses.
    ///
    /// An answer to an open that is no longer the pending one is dropped without a word
    /// (verifier (b) F2): it would open an editor over the one the reader is typing in, or
    /// print the refusal of a file they have stopped asking about.
    pub fn edit_read(
        &mut self,
        open: EditOpen,
        result: Result<Vec<u8>, Refused>,
    ) -> (Changed, Option<Effect>) {
        if self.edit_pending != Some(open.generation) {
            return (Changed::No, None);
        }
        self.edit_pending = None;
        let text = match result {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                // The engine already refused non-UTF-8; this is the belt to that braces, and
                // it says the same thing the engine would have.
                Err(_) => {
                    self.set_status(use_shift_i("binary"));
                    return (Changed::Yes, None);
                }
            },
            Err(Refused::NotEditable { why, .. }) => {
                self.set_status(use_shift_i(&why));
                return (Changed::Yes, None);
            }
            Err(r) => {
                self.set_status(r.message("opened"));
                return (Changed::Yes, None);
            }
        };
        let buf = TextBuf::open(&text, open.line);
        let last = buf.line_count() - 1;
        let mut marks = open.marks;
        marks.retain(|m| *m <= last);
        let band = open
            .band
            .filter(|(a, _)| *a <= last)
            .map(|(a, b)| (a, b.min(last)));
        self.editor = Some(Editor {
            root: open.root,
            rendered: open.rendered,
            buf,
            marks,
            band,
            line_at_open: open.line,
            saving: false,
            alarm: false,
        });
        self.clamp_editor();
        (Changed::Yes, None)
    }

    /// One keystroke inside the inline editor.
    ///
    /// Every key clears the red header a refused save left: the reader has seen it. The
    /// buffer owns the text, the cursor and the scroll; this owns the marks, which have to
    /// be told what the edit did to the line numbering — [`Editor::shift`] — and the
    /// clamp, which is the reducer's because the renderer may not move what it draws.
    fn editor_key(&mut self, key: EditorKey) -> (Changed, Option<Effect>) {
        let page = self.page_rows();
        let Some(ed) = self.editor.as_mut() else {
            return (Changed::No, None);
        };
        let seen = std::mem::replace(&mut ed.alarm, false);
        let mut changed = Changed::from(seen);
        match key {
            EditorKey::Edit(edit) => {
                let (lines_before, at_before) = (ed.buf.line_count(), ed.buf.cursor.line);
                let moved = ed.buf.apply(edit, page);
                let delta = ed.buf.line_count() as isize - lines_before as isize;
                // The edit's line is the *higher* of the two the caret sat on: a backspace
                // that joins two lines leaves the caret on the first, an inserted newline on
                // the second, and in both cases everything strictly below that line moved.
                ed.shift(at_before.min(ed.buf.cursor.line), delta);
                changed = changed.or(Changed::from(moved));
                self.clamp_editor();
                (changed, None)
            }
            EditorKey::Click(dy, dx) => {
                let (line, col) = (ed.buf.top + dy as usize, ed.buf.left + dx as usize);
                ed.buf.click(line, col);
                self.clamp_editor();
                (Changed::Yes, None)
            }
            EditorKey::Scroll(delta) => {
                let key = if delta < 0 {
                    EditKey::Up
                } else {
                    EditKey::Down
                };
                let mut moved = false;
                for _ in 0..delta.unsigned_abs() {
                    moved |= ed.buf.apply(key.clone(), page);
                }
                changed = changed.or(Changed::from(moved));
                self.clamp_editor();
                (changed, None)
            }
            EditorKey::Save => {
                if ed.saving {
                    return (changed, None);
                }
                ed.saving = true;
                let effect = Effect::Save {
                    root: ed.root.clone(),
                    rendered: ed.rendered.clone(),
                    bytes: ed.buf.text().into_bytes(),
                };
                (Changed::Yes, Some(effect))
            }
            EditorKey::Close => {
                if ed.buf.dirty() {
                    let path = ed.rendered.path.clone();
                    self.confirm = Some(Confirm {
                        scope: ConfirmScope::Discard { path },
                    });
                } else {
                    self.editor = None;
                }
                (Changed::Yes, None)
            }
        }
    }

    /// The columns the editor's text has, from the size the last `Resize` reported: the
    /// frame minus the nav, its borders and the line-number gutter.
    ///
    /// The renderer lays out from the frame's own area and this from `App::size`, which are
    /// the same rectangle — the loop feeds both from one resize event — so the window the
    /// reducer clamps to is the window the reader sees.
    fn editor_cols(&self) -> usize {
        usize::from(self.main_inner_cols())
            .saturating_sub(EDITOR_GUTTER)
            .max(1)
    }

    /// The diff pane's inner width: [`Self::editor_cols`] without the editor's gutter,
    /// which the diff does not have.
    ///
    /// The **fallback** for the body's columns when no frame has reported them yet
    /// (Phase 13). It is the same rectangle the renderer computes — `render` takes the
    /// main pane's `Block::inner`, and this is that arithmetic written out — so the width
    /// the reducer wraps at is the width the reader sees.
    pub(super) fn diff_cols(&self) -> usize {
        usize::from(self.main_inner_cols()).max(1)
    }

    /// The main pane's inner columns, from the size the last `Resize` reported: the frame
    /// minus the nav and the borders. Shared by [`Self::editor_cols`] and
    /// [`Self::diff_cols`] so the two can never drift apart.
    fn main_inner_cols(&self) -> u16 {
        if self.nav_visible() {
            let w = self.nav_width.min(self.size.0.saturating_sub(20));
            self.size.0.saturating_sub(w + 1)
        } else {
            self.size.0.saturating_sub(2)
        }
    }

    /// The diff body's `(columns, rows)` for the layout: what the last frame drew, or the
    /// fallback when no frame has reported one (Phase 13).
    pub(super) fn diff_body_size(&self) -> (u16, u16) {
        self.diff_size
            .unwrap_or((self.diff_cols() as u16, self.page_rows() as u16))
    }

    /// Scroll the editor's buffer so the caret is inside the window (F19). Called after
    /// every key and every resize; `render_editor` then draws the window as it stands.
    fn clamp_editor(&mut self) {
        let (rows, cols) = (self.page_rows(), self.editor_cols());
        if let Some(ed) = self.editor.as_mut() {
            ed.buf.viewport(rows, cols, Wrap::None);
        }
    }

    /// The loop's answer to an [`Effect::Save`] (deliverable 8; gate item 1 through the UI).
    ///
    /// A clean save is the whole point of the phase: the pile that comes back has the row
    /// **gone** (the override is the bytes that were just written), the editor closes, and
    /// the §6.7 advance rule moves the selection off the row exactly as an accept would.
    ///
    /// Every other answer keeps the buffer: a `Moved` means an agent wrote the file while
    /// the reader was typing, and throwing their text away to show them that would be the
    /// worst possible reading of "not saved" — so the buffer stays, the header goes red
    /// until the next key, and the status spells out the two keys that reload the file
    /// (F17 for the errors, which keep it for the same reason).
    pub fn saved(&mut self, root: PathBuf, path: Vec<u8>, result: SaveResult) -> Changed {
        let mine = self
            .editor
            .as_ref()
            .is_some_and(|e| e.rendered.path == path && e.root == root);
        if mine && let Some(ed) = self.editor.as_mut() {
            ed.saving = false;
        }
        let before = self.selection.clone();
        let name = String::from_utf8_lossy(&path).into_owned();
        match result {
            Ok(Saved {
                outcome, seq, pile, ..
            }) => {
                let refused = outcome.refused.first().cloned();
                self.apply_pile(root.clone(), seq, pile);
                match refused {
                    None => {
                        if mine {
                            self.editor = None;
                        }
                        self.advance_after(
                            &AcceptScope::File {
                                root,
                                path,
                                deleted: false,
                            },
                            before,
                            true,
                        );
                        self.set_status(format!("saved {name}"));
                    }
                    Some(Refused::Moved { .. }) => {
                        if let Some(ed) = self.editor.as_mut() {
                            ed.alarm = true;
                        }
                        self.set_status(save_refused_text(&name));
                    }
                    Some(other) => self.set_status(other.message("saved")),
                }
            }
            Err(AcceptFailed::LedgerBusy) => self.set_status(format!(
                "ledger busy in {} — try again",
                self.root_name(&root)
            )),
            Err(AcceptFailed::Other(e)) => {
                self.set_status(format!("{}: {e}", self.root_name(&root)))
            }
        }
        Changed::Yes
    }

    /// `shift-i`: hand the loop the suspend-and-open sequence, or say why there is nothing
    /// to open. Nothing is drawn on the way out — the next frame the user sees is either
    /// their editor or the resumed TUI — so a row that *can* be opened returns
    /// [`Changed::No`] and lets the loop's own status line speak on resume.
    fn edit_external(&mut self) -> (Changed, Option<Effect>) {
        if !matches!(self.selection, Some(Selection::Row(..))) {
            return (Changed::No, None);
        }
        match self.edit_target() {
            Some((root, rendered, line)) => (
                Changed::No,
                Some(Effect::EditExternal {
                    root,
                    rendered,
                    line,
                }),
            ),
            None => {
                self.set_status(NOT_EDITABLE);
                (Changed::Yes, None)
            }
        }
    }

    /// `m`: open the note modal on what `flag_target` names.
    fn open_note(&mut self) -> (Changed, Option<Effect>) {
        let Some(target) = self.flag_target() else {
            return (Changed::No, None);
        };
        self.note = Some(NoteEntry {
            target,
            buf: TextBuf::from(""),
        });
        (Changed::Yes, None)
    }

    /// One keystroke inside the note modal. Enter is the only exit that writes.
    ///
    /// Every edit goes to the buffer, which owns the cursor and the scroll: the modal has
    /// no text state of its own beyond it. A key the buffer answers "nothing moved" to is
    /// not a frame — `Home` at the start of a line redraws nothing.
    fn note_key(&mut self, key: NoteKey) -> (Changed, Option<Effect>) {
        let Some(entry) = self.note.as_mut() else {
            return (Changed::No, None);
        };
        match key {
            NoteKey::Edit(edit) => {
                let moved = entry.buf.apply(edit, NOTE_PAGE);
                (Changed::from(moved), None)
            }
            NoteKey::Cancel => {
                self.note = None;
                (Changed::Yes, None)
            }
            NoteKey::Send => {
                // The target was captured when `m` was pressed and is used as it was: a
                // pile that landed meanwhile cannot move the flag onto another hunk (F14).
                let entry = self.note.take().expect("checked above");
                let effect = Effect::Flag {
                    root: entry.target.root().to_path_buf(),
                    path: entry.target.path().to_vec(),
                    note: entry.text(),
                    hunk: entry.target.rendered_hunk(),
                    summary: entry.target.summary(),
                    label: entry.target.status_label(),
                };
                (Changed::Yes, Some(effect))
            }
        }
    }

    /// `shift-m`: clear every flag on the selected file.
    fn unflag_selected(&mut self) -> (Changed, Option<Effect>) {
        let Some(Selection::Row(root, path)) = self.selection.clone() else {
            return (Changed::No, None);
        };
        (Changed::Yes, Some(Effect::Unflag { root, path }))
    }

    /// One keystroke inside the agent picker.
    fn pick_key(&mut self, key: PickKey) -> (Changed, Option<Effect>) {
        let Some(picker) = self.picker.as_mut() else {
            return (Changed::No, None);
        };
        match key {
            PickKey::Up => {
                picker.selected = picker.selected.saturating_sub(1);
                (Changed::Yes, None)
            }
            PickKey::Down => {
                picker.selected = (picker.selected + 1).min(picker.candidates.len() - 1);
                (Changed::Yes, None)
            }
            PickKey::Cancel => {
                // The flag is already on disk; only the send is dropped.
                let label = self.picker.take().expect("checked above").label;
                self.set_status(format!("flagged {label} · not sent"));
                (Changed::Yes, None)
            }
            PickKey::Send => {
                let picker = self.picker.take().expect("checked above");
                let Some(agent) = picker.candidates.get(picker.selected).cloned() else {
                    self.set_status(format!("flagged {} · not sent", picker.label));
                    return (Changed::Yes, None);
                };
                self.set_status(format!(
                    "flagged {} · staged to {}",
                    picker.label, agent.label
                ));
                (
                    Changed::Yes,
                    Some(Effect::Stage {
                        pane_id: agent.pane_id,
                        flag: picker.label,
                        export: picker.export,
                    }),
                )
            }
        }
    }

    /// The loop's answer to an `Effect::Flag` or `Effect::Unflag`: the pile lands like any
    /// other, then the **send** is decided from the candidates the last derivation found —
    /// one agent stages straight away, several open the picker, none writes the export file.
    ///
    /// `kind` says which write this answers and, for a flag, what to call it. It comes from
    /// the effect the loop dispatched, so two writes in flight cannot be confused for one
    /// another (verifier (b) F2); an unflag is not a send that was cancelled.
    pub fn flagged(
        &mut self,
        root: PathBuf,
        kind: FlagKind,
        flagged: FlagResult,
    ) -> (Changed, Option<Effect>) {
        let flagged = match flagged {
            Ok(f) => f,
            Err(AcceptFailed::LedgerBusy) => {
                self.set_status(format!(
                    "ledger busy in {} — try again",
                    self.root_name(&root)
                ));
                return (Changed::Yes, None);
            }
            Err(AcceptFailed::Other(e)) => {
                self.set_status(format!("{}: {e}", self.root_name(&root)));
                return (Changed::Yes, None);
            }
        };
        self.apply_pile(root.clone(), flagged.seq, flagged.pile);
        let refusals: Vec<String> = flagged
            .outcome
            .refused
            .iter()
            .map(|r| r.message("flagged"))
            .collect();
        let FlagKind::Flag { label } = kind else {
            // An unflag: nothing to send, and the row's `⚑` is gone from the pile above.
            if refusals.is_empty() {
                self.set_status("flags cleared");
            } else {
                self.set_status(refusal_text(&refusals));
            }
            return (Changed::Yes, None);
        };
        if !refusals.is_empty() {
            self.set_status(refusal_text(&refusals));
            return (Changed::Yes, None);
        }
        if flagged.export.is_empty() {
            // Nothing was rendered to send — a refusal the engine did not classify, or a
            // path that flagged nothing. An empty export is never written to the day's
            // file as a blank entry (F2).
            self.set_status(format!("flagged {label} · not sent"));
            return (Changed::Yes, None);
        }
        let candidates = self.herdr.candidates(&root);
        match candidates.len() {
            0 => (
                Changed::Yes,
                Some(Effect::Export {
                    root,
                    label,
                    export: flagged.export,
                }),
            ),
            1 => {
                let agent = candidates.into_iter().next().expect("one candidate");
                self.set_status(format!("flagged {label} · staged to {}", agent.label));
                (
                    Changed::Yes,
                    Some(Effect::Stage {
                        pane_id: agent.pane_id,
                        // The words a failed send has to name: the flag is on disk either
                        // way, so the pane that would not take it is not the whole story.
                        flag: label,
                        export: flagged.export,
                    }),
                )
            }
            _ => {
                self.picker = Some(Picker {
                    root,
                    label,
                    export: flagged.export,
                    candidates,
                    selected: 0,
                });
                (Changed::Yes, None)
            }
        }
    }

    /// The loop's answer to an `Effect::Stage`: the flag is already on disk, so a failure
    /// is a status line and nothing more. `flag` came back with the answer (F2), so a
    /// second flag started meanwhile cannot lend this one its words.
    pub fn staged(&mut self, flag: String, result: Result<(), String>) -> Changed {
        match result {
            // The optimistic line is already on screen (`flagged` wrote it), so a success
            // has nothing to add.
            Ok(()) => Changed::No,
            Err(reason) => {
                self.set_status(format!("flagged {flag} · send failed: {reason}"));
                Changed::Yes
            }
        }
    }

    /// The loop's answer to an `Effect::Export`: the fallback file was written, or was not.
    /// `label` travelled with the effect for the same reason `staged`'s does (F2).
    pub fn exported(&mut self, label: String, result: Result<PathBuf, String>) -> Changed {
        match result {
            Ok(path) => self.set_status(format!("flagged {label} · export → {}", path.display())),
            Err(e) => self.set_status(format!("flagged {label} · export failed: {e}")),
        }
        Changed::Yes
    }

    /// `Restore`/`RestoreFile`: refuse while one runs, ask before a **file** restore (a
    /// whole row goes back, or an added file is removed), never before a hunk restore —
    /// the CAS is the guard and the content the hunk removes stays addressable in the
    /// private store (kickoff ruling item 2).
    fn request_restore(&mut self, scope: RestoreScope) -> (Changed, Option<Effect>) {
        if self.restoring.is_some() {
            self.set_status(RESTORE_IN_PROGRESS);
            return (Changed::Yes, None);
        }
        if self.restore_requests(&scope).is_empty() {
            self.set_status(NOTHING_TO_RESTORE);
            return (Changed::Yes, None);
        }
        // R2 (verifier F3): a row the folder never read has nothing to put back, so the
        // question would be a promise the engine refuses anyway. Say the engine's own
        // words and ask nothing.
        if self
            .roots
            .get(scope.root())
            .and_then(|v| v.row(scope.path()))
            .is_some_and(|r| matches!(r.collapsed, Some(Collapsed::Unread { .. })))
        {
            self.set_status(
                Refused::NotRead {
                    path: scope.path().to_vec(),
                }
                .message("restored"),
            );
            return (Changed::Yes, None);
        }
        if matches!(scope, RestoreScope::File { .. }) {
            self.confirm = Some(Confirm {
                scope: ConfirmScope::Restore(scope),
            });
            return (Changed::Yes, None);
        }
        self.start_restore(scope)
    }

    /// Build the request from the held view and hand it to the loop.
    fn start_restore(&mut self, scope: RestoreScope) -> (Changed, Option<Effect>) {
        let reqs = self.restore_requests(&scope);
        if reqs.is_empty() {
            self.set_status(NOTHING_TO_RESTORE);
            return (Changed::Yes, None);
        }
        self.restoring = Some(Restoring {
            scope: scope.clone(),
        });
        self.set_status("restoring…");
        (Changed::Yes, Some(Effect::Restore(reqs)))
    }

    // ---- undo (Amendment v1.11) ----------------------------------------------------------

    /// `z`: reverse the most recent accept in the selected root.
    ///
    /// The depth is read from the selected root's [`Pile::undo`] — the TUI never reads a
    /// ledger (design review F8) — so a stack another lastcall filled is `z`-able here the
    /// moment its pile lands, and an empty one is answered without a round trip.
    fn request_undo(&mut self) -> (Changed, Option<Effect>) {
        let Some(root) = self.flagged_root() else {
            return (Changed::No, None);
        };
        if self.undoing.is_some() {
            self.set_status(UNDO_IN_PROGRESS);
            return (Changed::Yes, None);
        }
        if self.roots.get(&root).is_some_and(|v| v.pile.undo == 0) {
            self.set_status(format!("{NOTHING_TO_UNDO} in {}", self.root_name(&root)));
            return (Changed::Yes, None);
        }
        self.undoing = Some(root.clone());
        self.set_status("undoing…");
        (Changed::Yes, Some(Effect::Undo(root)))
    }

    /// The loop's answer to an [`Effect::Undo`]: the pile takes the watcher path, the
    /// selection moves onto the first path the entry put back, `undoing` clears, and one
    /// status line says what came back.
    ///
    /// The selection move is the point of the whole gesture: the file the reader accepted
    /// by mistake has to be under the cursor again, not somewhere down the list. The focus
    /// is left where it was, so `z` from the diff pane leaves them reading the diff.
    pub fn undone(&mut self, root: PathBuf, result: UndoResult) -> Changed {
        let inflight = self.undoing.take();
        let mut changed = if inflight.is_some() {
            Changed::Yes
        } else {
            Changed::No
        };
        let name = self.root_name(&root);
        let mut parts: Vec<String> = Vec::new();
        match result {
            Ok(res) => {
                let refusals: Vec<String> = res
                    .outcome
                    .refused
                    .iter()
                    .map(|r| r.message("undone"))
                    .collect();
                let paths = res.paths.clone();
                changed = changed.or(self.apply_pile(root.clone(), res.seq, res.pile));
                if refusals.is_empty() && !paths.is_empty() {
                    parts.push(match paths.as_slice() {
                        [one] => format!("undid accept of {one}"),
                        many => format!("undid accept of {} in {name}", plural(many.len(), "file")),
                    });
                    if let Some(k) = self.other_undo_depth(&root) {
                        parts.push(format!("({k} other repos have their own undo)"));
                    }
                    self.select_undone(&root, &paths);
                } else if refusals.is_empty() {
                    parts.push(format!("{NOTHING_TO_UNDO} in {name}"));
                }
                parts.extend(refusals);
            }
            Err(AcceptFailed::LedgerBusy) => {
                parts.push(format!("ledger busy in {name} — try again"))
            }
            Err(AcceptFailed::Other(e)) => parts.push(format!("{name}: {e}")),
        }
        if inflight.is_none() && parts.is_empty() {
            return changed;
        }
        self.set_status(parts.join(" "));
        Changed::Yes
    }

    /// How many **other** repositories the last accept-all covered still have an undo entry
    /// of their own, or `None` when that is not what this undo was part of.
    ///
    /// `ctrl-a` across several roots writes one entry per root, so undoing in one of them
    /// leaves the rest accepted; the sentence says so rather than letting the reader
    /// believe `z` reached all of them. The roots are the ones the accept-all actually
    /// covered (recorded by [`App::accepted`]), filtered by what their piles report now, so
    /// a stack another process emptied meanwhile is not counted.
    fn other_undo_depth(&self, root: &Path) -> Option<usize> {
        let k = self
            .accept_all_roots
            .iter()
            .filter(|r| r.as_path() != root)
            .filter(|r| self.roots.get(*r).is_some_and(|v| v.pile.undo > 0))
            .count();
        (k > 0).then_some(k)
    }

    /// Put the cursor on the first path the undo put back, if the pile has it as a row.
    fn select_undone(&mut self, root: &Path, paths: &[String]) {
        let Some(first) = paths.first() else {
            return;
        };
        let bytes = first.as_bytes().to_vec();
        if self
            .roots
            .get(root)
            .is_some_and(|v| v.row(&bytes).is_some())
        {
            let focus = self.focus;
            self.select(Some(Selection::Row(root.to_path_buf(), bytes)));
            self.focus = focus;
        }
    }

    // ---- snooze (Amendment v1.11) --------------------------------------------------------

    /// `s`: open the snooze modal on the selected repository row, or wake a snoozed
    /// repository that `shift-s` is showing.
    ///
    /// Only a **repository row** answers: `s` on a file would have to guess which repository
    /// the reader meant, and guessing wrong hides work.
    fn snooze_selected(&mut self) -> (Changed, Option<Effect>) {
        let Some(Selection::Root(root)) = self.selection.clone() else {
            self.set_status(SNOOZE_NEEDS_ROOT);
            return (Changed::Yes, None);
        };
        if self.snoozing.is_some() {
            self.set_status(SNOOZE_IN_PROGRESS);
            return (Changed::Yes, None);
        }
        // Already snoozed (so `shift-s` is showing it): `s` wakes it, no modal. There is
        // nothing to ask — the answer to "for how long?" is "not at all".
        if self
            .roots
            .get(&root)
            .is_some_and(|v| v.pile.snoozed_until.is_some())
        {
            self.snoozing = Some(root.clone());
            return (Changed::Yes, Some(Effect::Snooze { root, days: None }));
        }
        self.snooze = Some(SnoozeEntry {
            name: self.root_name(&root),
            root,
            days: SNOOZE_DEFAULT_DAYS.to_string(),
        });
        (Changed::Yes, None)
    }

    /// One keystroke inside the snooze modal.
    /// One keystroke inside the first-launch welcome overlay (Amendment v1.11).
    ///
    /// Enter is the only key that does anything irreversible, and what it does depends on
    /// the row: the first row of a choice card keeps the default and advances, the second
    /// applies the change **to this session immediately** and asks the loop to remember it.
    /// The order matters — the setting is live whether or not the file can be written, so a
    /// read-only config directory costs a footer and not the choice.
    fn tour_key(&mut self, key: TourKey) -> (Changed, Option<Effect>) {
        let Some(tour) = &mut self.tour else {
            return (Changed::No, None);
        };
        match key {
            TourKey::Up | TourKey::Down => {
                let rows = tour.rows();
                // Nothing to move between on a plain card, and nothing to choose once a
                // failed write has replaced the footer: the choice is already applied.
                if rows == 0 || tour.failed.is_some() {
                    return (Changed::No, None);
                }
                let row = match key {
                    TourKey::Up => tour.row.saturating_sub(1),
                    _ => (tour.row + 1).min(rows - 1),
                };
                if row == tour.row {
                    return (Changed::No, None);
                }
                tour.row = row;
                (Changed::Yes, None)
            }
            TourKey::Skip => self.close_tour(),
            TourKey::Next => {
                // A failed write has said its piece on the footer; Enter moves on from it.
                let acknowledged = tour.failed.take().is_some();
                let setting = tour.card().setting().filter(|_| tour.row == 1);
                if acknowledged {
                    return self.advance_tour();
                }
                match setting {
                    Some(setting) => {
                        self.apply_tour_setting(setting);
                        (Changed::Yes, Some(Effect::TourWrite(setting)))
                    }
                    None => self.advance_tour(),
                }
            }
        }
    }

    /// The loop's answer to an [`Effect::TourWrite`]: the card advances when the file took
    /// the key, and keeps the screen with a sentence and the TOML line when it did not.
    pub fn tour_written(&mut self, result: Result<(), String>) -> (Changed, Option<Effect>) {
        match result {
            Ok(()) => self.advance_tour(),
            Err(message) => {
                let Some(tour) = &mut self.tour else {
                    return (Changed::No, None);
                };
                tour.failed = Some(message);
                (Changed::Yes, None)
            }
        }
    }

    /// The next card, or the end of the tour.
    ///
    /// The empty-repository card is built **here** rather than when the tour opened
    /// (deliverable 8, design review F4). The depth card sits in front of it and its second
    /// row rescans, so roots land between the two cards; a count taken at open would be the
    /// count from before them. What has landed by the time the reader presses `enter` is
    /// what the card says, and if that is under [`EMPTY_CARD_MIN`] there is no card.
    fn advance_tour(&mut self) -> (Changed, Option<Effect>) {
        if self.tour.is_none() {
            return (Changed::No, None);
        }
        let total = self.listed_roots().count();
        let empty = self.listed_roots().filter(|v| !v.listed()).count();
        let hide_empty = self.hide_empty;
        let Some(tour) = &mut self.tour else {
            return (Changed::No, None);
        };
        tour.at += 1;
        tour.row = 0;
        tour.failed = None;
        while let Some(Card::Empty { .. }) = tour.cards.get(tour.at) {
            if !hide_empty && empty >= EMPTY_CARD_MIN {
                tour.cards[tour.at] = Card::Empty { empty, total };
                break;
            }
            tour.at += 1;
        }
        if tour.at >= tour.cards.len() {
            return self.close_tour();
        }
        (Changed::Yes, None)
    }

    /// Close the overlay and ask the loop to write the marker. Every dismissal lands here
    /// but one: quitting with it open, which the loop notices after the event loop ends.
    fn close_tour(&mut self) -> (Changed, Option<Effect>) {
        self.tour = None;
        (Changed::Yes, Some(Effect::TourDone))
    }

    /// Apply a tour choice to **this session**, the same instant the write is asked for.
    ///
    /// Neither of these goes through its `Action`: `ScopeToggle` is a no-op when no scope
    /// was derived, and both cards are gated on the setting not being in force already, so
    /// a toggle and an assignment are the same thing here and the assignment is the one that
    /// says what it means.
    fn apply_tour_setting(&mut self, setting: Setting) {
        match setting {
            Setting::HerdrScopeAll => {
                self.herdr.scoped = false;
                self.reconcile_selection();
            }
            Setting::HideEmptyRepos => {
                self.hide_empty = true;
                self.reconcile_selection();
            }
            // Deliverable 8: the depth card changes nothing the reducer owns. The roots the
            // deeper walk finds arrive through the engine, on the rescan the loop asks for
            // once the depth is set, and reach the app as an ordinary roots update.
            Setting::SearchDepth2 => {}
        }
    }

    fn snooze_key(&mut self, key: SnoozeKey) -> (Changed, Option<Effect>) {
        let Some(entry) = &mut self.snooze else {
            return (Changed::No, None);
        };
        match key {
            SnoozeKey::Digit(c) => (entry.digit(c), None),
            SnoozeKey::Backspace => (entry.backspace(), None),
            SnoozeKey::Cancel => {
                self.snooze = None;
                (Changed::Yes, None)
            }
            SnoozeKey::Apply => {
                let entry = self.snooze.take().expect("checked above");
                let days = entry.value();
                self.snoozing = Some(entry.root.clone());
                (
                    Changed::Yes,
                    Some(Effect::Snooze {
                        root: entry.root,
                        days: Some(days),
                    }),
                )
            }
        }
    }

    /// The loop's answer to an [`Effect::Snooze`]: the pile takes the watcher path (which
    /// takes the repository off the nav and moves the selection through
    /// `reconcile_selection`), `snoozing` clears, and the status line names the deadline.
    pub fn snoozed_result(&mut self, root: PathBuf, result: SnoozeResult) -> Changed {
        let inflight = self.snoozing.take();
        let mut changed = if inflight.is_some() {
            Changed::Yes
        } else {
            Changed::No
        };
        let name = self.root_name(&root);
        let mut parts: Vec<String> = Vec::new();
        match result {
            Ok(res) => {
                parts.extend(res.outcome.refused.iter().map(|r| r.message("snoozed")));
                let until = res.until.clone();
                changed = changed.or(self.apply_pile(root, res.seq, res.pile));
                if parts.is_empty() {
                    parts.push(match &until {
                        Some(u) => format!("snoozed {name} until {}", iso8601_date(u)),
                        None => format!("woke {name}"),
                    });
                }
            }
            Err(AcceptFailed::LedgerBusy) => {
                parts.push(format!("ledger busy in {name} — try again"))
            }
            Err(AcceptFailed::Other(e)) => parts.push(format!("{name}: {e}")),
        }
        if inflight.is_none() && parts.is_empty() {
            return changed;
        }
        self.set_status(parts.join(" · "));
        Changed::Yes
    }

    /// Drop every held deadline the wall clock has passed (design review F4). `Changed::Yes`
    /// when at least one repository came back, which is also when the nav has to re-list.
    ///
    /// The engine clears the field on its own next ledger write and stamps an expired
    /// deadline as `None` at every scan; this is the same verdict applied to the piles the
    /// TUI is already holding, so a repository comes back within a second of its deadline
    /// rather than at the next scan or the next restart.
    fn drop_expired_snoozes(&mut self) -> Changed {
        let Some(wall) = self.wall else {
            return Changed::No;
        };
        let expired: Vec<PathBuf> = self
            .roots
            .iter()
            .filter(|(_, v)| {
                v.pile
                    .snoozed_until
                    .as_deref()
                    .and_then(parse_iso8601)
                    .is_some_and(|at| at <= wall)
            })
            .map(|(p, _)| p.clone())
            .collect();
        if expired.is_empty() {
            return Changed::No;
        }
        for root in expired {
            if let Some(view) = self.roots.get_mut(&root) {
                view.pile.snoozed_until = None;
            }
        }
        self.reconcile_selection();
        Changed::Yes
    }

    /// The loop's answer to an `Effect::Restore`: the pile goes through the same path as a
    /// watcher pile (seq included), the §6.7 advance rule runs for the selection the restore
    /// was asked from, `restoring` clears and one status line says what happened.
    ///
    /// Refusals are worded with `restored`, not `accepted`: nothing was written, and the
    /// sentence has to say which operation did not happen.
    pub fn restored(&mut self, results: Vec<(PathBuf, RestoreResult)>) -> Changed {
        let inflight = self.restoring.take();
        let before = self.selection.clone();
        let mut changed = if inflight.is_some() {
            Changed::Yes
        } else {
            Changed::No
        };
        let mut refusals: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let mut ok = false;
        for (root, result) in results {
            match result {
                Ok(res) => {
                    refusals.extend(res.outcome.refused.iter().map(|r| r.message("restored")));
                    ok = true;
                    changed = changed.or(self.apply_pile(root, res.seq, res.pile));
                }
                Err(AcceptFailed::LedgerBusy) => errors.push(format!(
                    "ledger busy in {} — try again",
                    self.root_name(&root)
                )),
                Err(AcceptFailed::Other(e)) => {
                    errors.push(format!("{}: {e}", self.root_name(&root)))
                }
            }
        }
        let Some(Restoring { scope }) = inflight else {
            return changed;
        };
        let taken = refusals.is_empty() && errors.is_empty();
        self.advance_after_restore(&scope, before, taken);
        let mut parts = Vec::new();
        if taken && ok {
            parts.push(restored_text(&scope));
        }
        if !refusals.is_empty() {
            parts.push(refusal_text(&refusals));
        }
        parts.extend(errors);
        self.set_status(parts.join(" · "));
        Changed::Yes
    }

    /// §6.7 after a restore's pile came back, the accept rule with one scope translated:
    /// the row the restore was asked on is gone → advance from it; a hunk restore that was
    /// taken and left hunks in the row keeps the cursor index, clamped.
    fn advance_after_restore(
        &mut self,
        scope: &RestoreScope,
        before: Option<Selection>,
        taken: bool,
    ) {
        let equivalent = match scope {
            RestoreScope::Hunk {
                root,
                path,
                index,
                hunks,
            } => AcceptScope::Hunk {
                root: root.clone(),
                path: path.clone(),
                index: *index,
                hunks: *hunks,
            },
            RestoreScope::File {
                root,
                path,
                deleted,
                ..
            } => AcceptScope::File {
                root: root.clone(),
                path: path.clone(),
                deleted: *deleted,
            },
        };
        self.advance_after(&equivalent, before, taken);
    }

    /// The loop's answer to an `Effect::Accept`: every root's pile goes through the same
    /// path as a watcher pile (seq included), then the §6.7 advance rule runs for the
    /// selection the accept was asked from, `accepting` clears and one status line says
    /// what happened. An `Err` for one root is named in the status and undoes nothing.
    pub fn accepted(&mut self, results: Vec<(PathBuf, AcceptResult)>) -> Changed {
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
                // A busy ledger is a whole-root condition, not a per-row refusal: another
                // lastcall process holds this root's lock. Say so plainly and leave the row
                // pending — the same keystroke works a moment later (Phase 5 deliverable 2c).
                Err(AcceptFailed::LedgerBusy) => errors.push(format!(
                    "ledger busy in {} — try again",
                    self.root_name(&root)
                )),
                Err(AcceptFailed::Other(e)) => {
                    errors.push(format!("{}: {e}", self.root_name(&root)))
                }
            }
        }
        let Some(Accepting { scope, files }) = inflight else {
            return changed;
        };
        let taken = refusals.is_empty() && errors.is_empty();
        // Amendment v1.11: an accept-all that folded more than one repository left an undo
        // entry on each of them, and the next `z` says so.
        if matches!(scope, AcceptScope::All) {
            self.accept_all_roots = if ok_roots.len() > 1 {
                ok_roots.clone()
            } else {
                Vec::new()
            };
        }
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
            // The ruling: say what is left, not the cursor's slot (which is 1 again once
            // the accepted hunk slides out). `hunks` is the count the row showed when the
            // accept was asked, so one fewer remains.
            AcceptScope::Hunk { path, hunks, .. } => match hunks.saturating_sub(1) {
                0 => format!("accepted {} · file complete", lossy(path)),
                left => format!("accepted {} · {} left", lossy(path), plural(left, "hunk")),
            },
            AcceptScope::File { path, deleted, .. } => {
                let suffix = if *deleted { " (deleted)" } else { "" };
                format!("accepted {}{suffix}", lossy(path))
            }
            // The blessing says `reviewed`, not `accepted`: what was marked seen is the
            // content the user's own editor session left, which is the whole point of the
            // confirm they just answered (ruling P1).
            AcceptScope::Bless { path, .. } => format!("reviewed {}", lossy(path)),
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

    /// §6.7 after the piles came back. There is one neighbour rule this phase
    /// ([`App::neighbour_after`]) and `apply_pile`'s `reconcile_selection` has already run
    /// it, so a vanished selection needs nothing more here — the second, differently
    /// ordered `advance` this method used to own is gone (kickoff deliverable 2.3). The
    /// `reconcile_selection` below is the same rule again, for the one caller that reaches
    /// here without a pile having landed for the selection's root.
    ///
    /// What is still this method's own: a hunk accept that was `taken` (no refusal, no
    /// error) and left hunks in the row keeps the cursor index (clamped) and scrolls to it.
    /// A refused one moves nothing — the scroll stays where the user had it, not at the
    /// hunk header.
    fn advance_after(&mut self, scope: &AcceptScope, before: Option<Selection>, taken: bool) {
        let Some(before) = before else {
            return;
        };
        if !self.nav_entries().contains(&before) {
            if self.selection.as_ref() == Some(&before) {
                self.reconcile_selection();
            }
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
    /// Either way the nav index is snapshotted (design review F7): the neighbour rule needs
    /// the order **as it was while this entry was selected**, and by the time it runs
    /// `apply_pile` has already replaced the pile the order came from.
    pub fn select(&mut self, next: Option<Selection>) -> Changed {
        let anchor = next
            .as_ref()
            .and_then(|s| self.nav_entries().iter().position(|e| e == s));
        self.nav_anchor = anchor;
        if self.selection == next {
            return Changed::No;
        }
        self.selection = next;
        self.diff = DiffCursor::default();
        // A selection is a range of *this* row's diff lines; the moment the row changes the
        // range means nothing, so it goes rather than pointing at another file's text.
        self.sel = None;
        self.drop_stale_expansion();
        Changed::Yes
    }

    /// After roots or piles changed: keep the selection if it still exists (clamping the
    /// diff cursor and re-anchoring it in the new order), else move to
    /// [`App::neighbour_after`]. Every path that can lose the selected entry — a pile, an
    /// accept, a restore, `t`, `w`, a herdr scope or roots update, the end of the launch
    /// hold — comes through here, so the rule is stated once.
    pub fn reconcile_selection(&mut self) {
        let Some(sel) = self.selection.clone() else {
            return;
        };
        let entries = self.nav_entries();
        if let Some(at) = entries.iter().position(|e| *e == sel) {
            self.nav_anchor = Some(at);
            self.clamp_cursor();
            return;
        }
        let next = self.neighbour_after(&sel, &entries);
        self.select(next);
    }

    /// Where the cursor goes when `gone` has left the nav (§6.7, Amendment v1.9 item 4;
    /// ruling R2), against the nav order `entries`:
    ///
    /// 1. the nearest surviving entry **below** it **within the same repo**;
    /// 2. else the nearest **above** within the same repo — the repo's own name row is the
    ///    last "above", so **accepting the last file lands on the repo row**;
    /// 3. else — only once the repo itself has left the nav, which `hide_empty` or the
    ///    scope is the only way to arrange — the entry now standing at `gone`'s former nav
    ///    index ([`App::nav_anchor`]), else the previous one, else nothing.
    ///
    /// Never a wrap back to the repo's first row, and never a jump into another repo while
    /// this one is still listed. "Below" and "above" are read from [`nav_key`], not from
    /// index arithmetic, because a pile that *added* rows above the vanished one moves
    /// every index while changing nothing about what is below it.
    fn neighbour_after(&self, gone: &Selection, entries: &[Selection]) -> Option<Selection> {
        let root = gone.root();
        let key = nav_key(gone);
        let mine: Vec<&Selection> = entries.iter().filter(|e| e.root() == root).collect();
        if !mine.is_empty() {
            return mine
                .iter()
                .find(|e| nav_key(e) > key)
                .or_else(|| mine.iter().rev().find(|e| nav_key(e) < key))
                .map(|e| (*e).clone());
        }
        let at = self.nav_anchor.unwrap_or(0);
        entries
            .get(at)
            .or_else(|| entries.get(at.min(entries.len()).saturating_sub(1)))
            .cloned()
    }

    fn clamp_cursor(&mut self) {
        if self.selected_row().is_none() {
            self.diff = DiffCursor::default();
            return;
        }
        let (hunks, lines) = {
            let h = self.view_hunks();
            (h.len(), diff_lines(h))
        };
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

    /// `nav_top` / `nav_bottom` with the nav focused: the first or the last entry.
    ///
    /// The entry comes out of [`Self::nav_entries`] and goes through [`Self::select`],
    /// which is [`Self::move_selection`]'s own last step: one selection path, so the diff,
    /// the expansion and an empty nav all answer exactly as `↑`/`↓` make them. What it
    /// does *not* borrow is `move_selection`'s rule for a nav with nothing selected yet —
    /// there `↓` starts at the top, and `end` means the end whichever key came first.
    fn jump_end(&mut self, down: bool) -> Changed {
        let entries = self.nav_entries();
        let target = if down {
            entries.last()
        } else {
            entries.first()
        };
        let target = target.cloned();
        self.select(target)
    }

    /// `nav_top` / `nav_bottom` with the diff focused: the first line, or the last line a
    /// long `↓` run reaches — [`Self::scroll_by`] clamps to the same place, so this is a
    /// move of the whole diff's length and not a second scroll path. While a selection is
    /// running it moves the selection's far end instead, exactly as the page keys do.
    fn jump_diff_end(&mut self, down: bool) -> Changed {
        let span = diff_lines(self.view_hunks()) as isize;
        let delta = if down { span } else { -span };
        if self.sel.is_some() {
            self.move_sel_cursor(delta)
        } else {
            self.scroll_by(delta)
        }
    }

    /// `nav_prev_root` / `nav_next_root`: the repository row of the listed root before or
    /// after the selection's own ([`Selection::root`]).
    ///
    /// The roots come out of [`Self::nav_entries`], so one that the scope, `t` or a snooze
    /// has taken off the nav is skipped the way `↑`/`↓` skip it. There is no wrap: on the
    /// last root `}` changes nothing at all, and `{` inside the first selects that root's
    /// own row, which is where a reader deep inside it means to land. A repository row has
    /// no diff, so the keys come back to the nav with the selection.
    fn jump_root(&mut self, forward: bool) -> Changed {
        let roots: Vec<PathBuf> = self
            .nav_entries()
            .into_iter()
            .filter_map(|e| match e {
                Selection::Root(root) => Some(root),
                _ => None,
            })
            .collect();
        let Some(first) = roots.first() else {
            return self.select(None);
        };
        let at = self
            .selection
            .as_ref()
            .and_then(|s| roots.iter().position(|r| r.as_path() == s.root()));
        let target = match (at, forward) {
            (Some(i), true) => match roots.get(i + 1) {
                Some(root) => root.clone(),
                None => return Changed::No,
            },
            (Some(i), false) => roots[i.saturating_sub(1)].clone(),
            // Nothing selected yet: both keys start at the nav's first repository row.
            (None, _) => first.clone(),
        };
        let moved = self.select(Some(Selection::Root(target)));
        moved.or(self.set_focus(Focus::Nav))
    }

    // ---- diff cursor ---------------------------------------------------------------------

    fn scroll_by(&mut self, delta: isize) -> Changed {
        if self.selected_row().is_none() {
            return Changed::No;
        }
        let max = diff_lines(self.view_hunks()).saturating_sub(1) as isize;
        let next = (self.diff.scroll as isize + delta).clamp(0, max) as usize;
        if next == self.diff.scroll {
            return Changed::No;
        }
        self.diff.scroll = next;
        Changed::Yes
    }

    /// A forward scroll of `delta` lines that cannot skip one (Phase 13, design review F13
    /// and F16).
    ///
    /// A line may now be several rows tall, so "scroll by a page" can no longer mean "add
    /// the body's row count to the line index": that would step straight over lines that
    /// were never drawn. The new top is at most **one past the last line that was fully on
    /// screen**, which is exactly one line forward when only the top line fitted, and it
    /// always moves at least one line so a page key at a tall line still makes progress.
    /// [`Self::scroll_by`] does the clamping to the diff's end, unchanged.
    fn scroll_forward(&mut self, delta: usize) -> Changed {
        let at = self.diff.scroll;
        let target = {
            let (cols, rows) = self.diff_body_size();
            let hunks = self.view_hunks();
            let last = wrap::last_full_line(hunks, at, cols, rows, self.wrap).unwrap_or(at);
            at.saturating_add(delta).min(last + 1).max(at + 1)
        };
        self.scroll_by(target as isize - at as isize)
    }

    /// A page up: the lowest top line from which the current top line is still **fully** on
    /// screen (a backward fill), and at least one line.
    ///
    /// Not page down's inverse. With rows of different heights the two cannot be, and no
    /// test may claim they are; what page up promises is that the line the reader was
    /// looking at is still whole on the screen they land on.
    fn scroll_page_up(&mut self) -> Changed {
        let at = self.diff.scroll;
        let target = {
            let (cols, rows) = self.diff_body_size();
            let hunks = self.view_hunks();
            wrap::page_up_top(hunks, at, cols, rows, self.wrap)
        };
        self.scroll_by(target as isize - at as isize)
    }

    /// Move a live selection's far end `delta` lines and scroll only as far as it takes to
    /// keep that end on screen.
    ///
    /// This is what `nav_up`/`nav_down`/`page` do while a selection is running, in place of
    /// [`Self::scroll_by`]. The diff pane has no per-line cursor of its own — its cursor
    /// *is* the first visible line — and moving that as the selection grew would push the
    /// lines being selected off the top of the pane, one per keystroke. So while `v` is
    /// live the selection's own end is the cursor, and `v j j y` copies the three lines the
    /// reader can see.
    fn move_sel_cursor(&mut self, delta: isize) -> Changed {
        let Some(sel) = self.sel else {
            return Changed::No;
        };
        let max = diff_lines(self.view_hunks()).saturating_sub(1) as isize;
        let next = (sel.cursor as isize + delta).clamp(0, max) as usize;
        if next == sel.cursor {
            return Changed::No;
        }
        self.sel = Some(Sel {
            cursor: next,
            ..sel
        });
        // Phase 13: "on screen" is a statement about rows, and a line can be several of
        // them, so the smallest scroll that keeps the moving end **whole** comes from the
        // same layout the renderer draws — not from a line count against `page_rows`.
        self.diff.scroll = {
            let at = self.diff.scroll;
            let (cols, rows) = self.diff_body_size();
            let hunks = self.view_hunks();
            wrap::keep_visible(hunks, at, next, cols, rows, self.wrap)
        };
        Changed::Yes
    }

    /// Move the hunk cursor and scroll so its header is the first visible line.
    fn move_hunk(&mut self, delta: isize) -> Changed {
        let (len, offsets) = {
            let h = self.view_hunks();
            (h.len(), hunk_offsets(h))
        };
        if len == 0 {
            return Changed::No;
        }
        let max = len as isize - 1;
        let next = (self.diff.hunk as isize + delta).clamp(0, max) as usize;
        let scroll = offsets[next];
        if next == self.diff.hunk && scroll == self.diff.scroll {
            return Changed::No;
        }
        self.diff.hunk = next;
        self.diff.scroll = scroll;
        self.follow_selection();
        Changed::Yes
    }

    // ---- select to copy (deliverable 9) ---------------------------------------------------

    /// Drag a live selection's moving end along with the diff cursor. Called by everything
    /// that moves the cursor — the nav keys, a page, `hunk_next`, the wheel over the diff —
    /// because the cursor *is* the selection's far end, whatever moved it. A selection that
    /// began with the mouse is taken over by the keyboard here rather than being dropped:
    /// the anchor is still where the reader put it.
    fn follow_selection(&mut self) {
        if let Some(sel) = &mut self.sel {
            sel.cursor = self.diff.scroll;
        }
    }

    /// `v`: anchor a selection at the diff cursor, or, with one already running, pull its
    /// moving end to the cursor. There is no per-line cursor in the diff pane — the cursor
    /// *is* [`DiffCursor::scroll`], the first visible line — so `v j j y` copies three
    /// lines, which is the sequence the kickoff names.
    fn start_selection(&mut self) -> Changed {
        if self.selected_row().is_none() || self.view_hunks().is_empty() {
            return Changed::No;
        }
        let at = self.diff.scroll;
        let next = Sel {
            anchor: self.sel.map_or(at, |s| s.anchor),
            cursor: at,
        };
        if self.sel == Some(next) {
            return Changed::No;
        }
        self.sel = Some(next);
        Changed::Yes
    }

    /// The bytes `y` would put on the clipboard: the selection's lines, or — with no
    /// selection — the hunk under the cursor whole, header included.
    pub fn copy_payload(&self) -> Option<Vec<u8>> {
        let hunks = self.view_hunks();
        if hunks.is_empty() {
            return None;
        }
        let Some(sel) = self.sel else {
            let hunk = hunks.get(self.diff.hunk.min(hunks.len() - 1))?;
            return Some(format!("{}\n{}", hunk_header(hunk), hunk_body(hunk)).into_bytes());
        };
        let last = diff_lines(hunks).checked_sub(1)?;
        let (a, b) = sel.range();
        let mut out = String::new();
        for i in a.min(last)..=b.min(last) {
            out.push_str(&diff_line_text(hunks, i)?);
            out.push('\n');
        }
        Some(out.into_bytes())
    }

    /// `y`, and the mouse release that ends a drag: copy, clear the selection, raise the
    /// cue. Over the cap nothing is written and the selection **stays**, because the only
    /// thing the reader can do about it is select less and they need the range to shrink.
    fn copy_selection(&mut self) -> (Changed, Option<Effect>) {
        let Some(bytes) = self.copy_payload() else {
            return (Changed::No, None);
        };
        if bytes.len() > super::clipboard::CAP {
            self.set_status(too_large_text(bytes.len()));
            return (Changed::Yes, None);
        }
        self.sel = None;
        // The status line is the record of what the engine did; a copy is a thing the
        // terminal did, so it gets its own cue and leaves an accept's or a refusal's
        // sentence on screen.
        self.cue = Some(Cue {
            text: COPIED.to_owned(),
            until: self.now + Duration::from_secs(CUE_SECS),
        });
        (Changed::Yes, Some(Effect::Copy(bytes)))
    }

    // ---- user actions --------------------------------------------------------------------

    /// Fold one user action in.
    pub fn handle(&mut self, action: Action) -> (Changed, Option<Effect>) {
        use Action::*;
        // The first-launch tour is above everything, the help overlay and the confirm modal
        // included (F7): a `?` before the welcome is dismissed must not open help *under*
        // it, and a card with a highlighted row is a question that has to be answered or
        // skipped before anything else happens. `Ui::event` resolves every key through
        // `tour_action` first, so the only keystrokes that reach here are its own and a
        // non-printable quit; `Press` reaches here as the no-op it always is (the loop
        // resolves it through the hit map), and `Tick`, `Resize` and `Herdr` pass because
        // none of them is a keystroke.
        if self.tour.is_some()
            && !matches!(
                action,
                Tick | Resize(..) | Tour(_) | Quit | Herdr(_) | Press(..)
            )
        {
            return (Changed::No, None);
        }
        // The note modal owns the keyboard: `Ui::event` resolves every key through
        // `note_action` before the keymap, so the only actions that reach here are its own
        // edits, a quit, and the events that pass through every modal.
        if self.note.is_some() && !matches!(action, Tick | Resize(..) | Note(_) | Quit | Herdr(_)) {
            return (Changed::No, None);
        }
        // The picker, on the same terms. A `Herdr` update re-derives its candidates below.
        if self.picker.is_some() && !matches!(action, Tick | Resize(..) | Pick(_) | Quit | Herdr(_))
        {
            return (Changed::No, None);
        }
        // The snooze modal, on the note modal's terms: `Ui::event` resolves every key
        // through `snooze_action` first, so only its own edits, a quit and the events that
        // pass through every modal reach here.
        if self.snooze.is_some()
            && !matches!(action, Tick | Resize(..) | SnoozeEdit(_) | Quit | Herdr(_))
        {
            return (Changed::No, None);
        }
        // `Herdr` passes both gates: news from the herdr task is not a keystroke, and it
        // must never close the confirm modal or the help overlay (deliverable 5).
        if self.confirm.is_some()
            && !matches!(
                action,
                Tick | Resize(..) | Confirm | Cancel | Quit | Herdr(_)
            )
        {
            return (Changed::No, None);
        }
        if let Herdr(update) = action {
            return self.herdr_update(update);
        }
        // `Tour(_)` passes for the same reason `Quit` does: the welcome sits above the help
        // overlay, so its own keys must act on the card rather than spend themselves closing
        // help underneath it. `Plan::open` closes help as the welcome opens, so the reader
        // never sees the two together; this arm is what keeps a card's first keystroke from
        // being swallowed if it ever does.
        if self.help
            && !matches!(
                action,
                Tick | Resize(..) | Drag(..) | Release | Press(..) | Quit | Tour(_)
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
            NavTop if nav => self.jump_end(false),
            NavBottom if nav => self.jump_end(true),
            // The two ends of the diff, and the whole of a live selection with them.
            NavTop => self.jump_diff_end(false),
            NavBottom => self.jump_diff_end(true),
            // The repository jumps read the same in both panes, which is why they are not
            // split by focus: they always land on a repository row, and they always leave
            // the keys in the nav.
            NavPrevRoot => self.jump_root(false),
            NavNextRoot => self.jump_root(true),
            // In the diff with a selection running, these keys move its far end
            // (deliverable 9); with none, they scroll the pane as they always have.
            NavUp if self.sel.is_some() => self.move_sel_cursor(-1),
            NavDown if self.sel.is_some() => self.move_sel_cursor(1),
            NavPageUp if self.sel.is_some() => self.move_sel_cursor(-page),
            NavPageDown if self.sel.is_some() => self.move_sel_cursor(page),
            ScrollUp(n) if self.sel.is_some() => self.move_sel_cursor(-(n as isize)),
            ScrollDown(n) if self.sel.is_some() => self.move_sel_cursor(n as isize),
            NavUp => self.scroll_by(-1),
            NavDown => self.scroll_by(1),
            // Phase 13: the forward moves are clamped so they cannot step over a line that
            // was never drawn, and page up is a backward fill rather than page down's
            // inverse. Backward by `n` cannot skip anything — every line between the old
            // and the new top comes on screen — so the wheel up stays a plain move.
            NavPageUp => self.scroll_page_up(),
            NavPageDown => self.scroll_forward(page as usize),
            ScrollUp(n) => self.scroll_by(-(n as isize)),
            ScrollDown(n) => self.scroll_forward(n as usize),
            Open => match self.selection.clone() {
                Some(Selection::Row(..)) | Some(Selection::Group(..)) => {
                    self.set_focus(Focus::Diff)
                }
                Some(Selection::Root(root)) => {
                    let first = self
                        .nav_entries()
                        .into_iter()
                        .find(|e| matches!(e, Selection::Row(r, _) if *r == root));
                    match first {
                        Some(row) => self.select(Some(row)),
                        // A flag-only root has no row to open; the diff pane says
                        // `nothing pending · agent done`, so focus it rather than
                        // dropping the selection.
                        None => self.set_focus(Focus::Diff),
                    }
                }
                None => self.move_selection(1),
            },
            // Esc peels one layer: a live selection first (cleared, never copied), and only
            // then the focus. A reader who selected by mistake gets out of it without
            // losing the pane they were reading.
            Back if self.sel.is_some() => {
                self.sel = None;
                Changed::Yes
            }
            Back => self.set_focus(Focus::Nav),
            Select if self.effective_focus() == Focus::Diff => self.start_selection(),
            Select => Changed::No,
            Copy if self.effective_focus() == Focus::Diff => return self.copy_selection(),
            Copy => Changed::No,
            // Only a drag whose press landed in the diff body selects; `press_line` is set
            // by the loop from the last frame's rectangle, so a drag that began on the
            // divider is still a divider drag (design review F13).
            SelectTo(line) => match self.press_line {
                Some(anchor) => {
                    let last = diff_lines(self.view_hunks()).saturating_sub(1);
                    let cursor = line.min(last);
                    let next = Sel {
                        anchor: anchor.min(last),
                        cursor,
                    };
                    if cursor != next.anchor {
                        self.drag_moved = true;
                    }
                    if self.sel == Some(next) {
                        return (Changed::No, None);
                    }
                    self.sel = Some(next);
                    Changed::Yes
                }
                None => Changed::No,
            },
            FocusToggle => match self.focus {
                Focus::Nav => self.set_focus(Focus::Diff),
                Focus::Diff => self.set_focus(Focus::Nav),
            },
            HunkNext => self.move_hunk(1),
            HunkPrev => self.move_hunk(-1),
            Expand => return self.request_expand(),
            ToggleFullPaths => {
                self.full_paths = !self.full_paths;
                Changed::Yes
            }
            ToggleRemote => {
                self.show_remote = !self.show_remote;
                Changed::Yes
            }
            // `t` (§6.7, Amendment v1.9 item 4). Independent of `w`: this hides the repos
            // with nothing pending among the ones the scope allows, and never brings a
            // scoped-out repo back.
            HideEmpty => {
                self.hide_empty = !self.hide_empty;
                self.reconcile_selection();
                Changed::Yes
            }
            // Phase 13: the scroll is a line index and keeps its value, so the line the
            // reader was at is still the top line after the flip (ruling 1). Nothing is
            // written to disk: `[ui] wrap` is the opening answer, this is the session's.
            ToggleWrap => {
                self.wrap = !self.wrap;
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
                Some(AcceptAnswer::Take(scope)) => return self.request_accept(scope),
                // The status line changed, so the frame did: `Changed::Yes`.
                Some(AcceptAnswer::Refuse(text)) => {
                    self.set_status(text);
                    Changed::Yes
                }
                None => Changed::No,
            },
            AcceptFile => match self.accept_file_scope() {
                Some(scope) => return self.request_accept(scope),
                None => Changed::No,
            },
            AcceptAll => return self.request_accept(AcceptScope::All),
            Restore => match self.restore_scope() {
                Some(scope) => return self.request_restore(scope),
                None => Changed::No,
            },
            RestoreFile => match self.restore_file_scope() {
                Some(scope) => return self.request_restore(scope),
                None => Changed::No,
            },
            Flag => return self.open_note(),
            Unflag => return self.unflag_selected(),
            Undo => return self.request_undo(),
            Snooze => return self.snooze_selected(),
            ShowSnoozed => {
                self.show_snoozed = !self.show_snoozed;
                self.reconcile_selection();
                Changed::Yes
            }
            SnoozeEdit(key) => return self.snooze_key(key),
            Tour(key) => return self.tour_key(key),
            Edit => return self.edit_inline(),
            EditExternal => return self.edit_external(),
            Editor(key) => return self.editor_key(key),
            Note(key) => return self.note_key(key),
            Pick(key) => return self.pick_key(key),
            // `Confirm` here is `Action::Confirm` (`use Action::*` above), so the modal's
            // own scope is matched inside rather than in the pattern.
            Confirm => match self.confirm.clone().map(|c| c.scope) {
                Some(ConfirmScope::Accept(_)) if self.accepting.is_some() => {
                    // Re-checked here: the modal stays open, the status says why.
                    self.set_status(ACCEPT_IN_PROGRESS);
                    Changed::Yes
                }
                Some(ConfirmScope::Restore(_)) if self.restoring.is_some() => {
                    self.set_status(RESTORE_IN_PROGRESS);
                    Changed::Yes
                }
                Some(ConfirmScope::Accept(scope)) => {
                    self.confirm = None;
                    return self.start_accept(scope);
                }
                Some(ConfirmScope::Restore(scope)) => {
                    self.confirm = None;
                    return self.start_restore(scope);
                }
                // The one confirm that runs no op: `y` throws the buffer away and closes
                // the editor behind it, and nothing on disk was ever touched.
                Some(ConfirmScope::Discard { .. }) => {
                    self.confirm = None;
                    self.editor = None;
                    Changed::Yes
                }
                None => Changed::No,
            },
            Cancel => {
                // A declined blessing is worth a sentence: the user has just been asked a
                // question about a file, and silence would leave them guessing whether the
                // `n` landed. The row itself stays, showing the editor's delta like any
                // other pending change.
                let declined = self
                    .confirm_bless()
                    .map(|p| String::from_utf8_lossy(p).into_owned());
                if self.confirm.take().is_some() {
                    if let Some(path) = declined {
                        self.set_status(format!("{path} left pending"));
                    }
                    Changed::Yes
                } else {
                    Changed::No
                }
            }
            Ack => return self.ack_selected(),
            Jump => return self.jump_selected(),
            ScopeToggle => {
                if self.herdr.scope.is_none() {
                    return (Changed::No, None);
                }
                self.herdr.scoped = !self.herdr.scoped;
                self.reconcile_selection();
                Changed::Yes
            }
            Herdr(_) => unreachable!("handled above, before the help gate"),
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
                let anchor = self.press_line.take();
                let dragged = std::mem::take(&mut self.drag_moved);
                // A press and a release with nothing in between is a click, not a
                // zero-length copy — and this is state, not a comparison of timestamps, so
                // it holds when both arrive in one drained pass.
                if dragged && anchor.is_some() && self.sel.is_some() {
                    return self.copy_selection();
                }
                Changed::No
            }
            Resize(w, h) => {
                self.size = (w, h);
                // Phase 13: the body's reported size belongs to the frame that drew it,
                // and that frame is now the wrong shape. `run.rs` drops the hit map at the
                // same point for the same reason; until the next draw the layout uses the
                // fallback rather than a stale rectangle.
                self.diff_size = None;
                // F19: the editor's window is the frame's, so a resize that shrinks it must
                // scroll the caret back into view before the next draw.
                self.clamp_editor();
                Changed::Yes
            }
            Tick => {
                self.now += Duration::from_secs(1);
                // The cue is on its own clock, so it goes when its two seconds are up
                // whatever the status line is doing.
                let cue_went = match &self.cue {
                    Some(c) if self.now >= c.until => {
                        self.cue = None;
                        true
                    }
                    _ => false,
                };
                let status = match &self.status {
                    Some(s) if self.now.duration_since(s.at) >= STATUS_TTL => {
                        self.status = None; // the hints come back
                        Changed::Yes
                    }
                    Some(_) => Changed::Yes,
                    None => Changed::No,
                };
                // Amendment v1.11 / design review F4: a snooze that runs out while the
                // TUI is open comes back here rather than at the next scan, and the
                // comparison is against the engine's clock as the loop last reported it —
                // the reducer owns the decision, `render` never sees a deadline that is
                // already past.
                let woke = self.drop_expired_snoozes();
                // The loading pane carries a seconds counter.
                if woke == Changed::Yes || cue_went || self.loading.is_some() {
                    Changed::Yes
                } else {
                    status
                }
            }
        };
        (changed, None)
    }

    // ---- herdr ---------------------------------------------------------------------------

    /// The root an ack or a jump applies to: the selected entry's root, whichever pane has
    /// focus and whether a root, a row or a group is selected.
    pub fn flagged_root(&self) -> Option<PathBuf> {
        Some(self.selection.as_ref()?.root().to_path_buf())
    }

    /// `d`: ack the selected root's flag. Local to this process (ruling 10) and idempotent;
    /// the dot dims and herdr is not told. The name also leaves any pending toast window.
    fn ack_selected(&mut self) -> (Changed, Option<Effect>) {
        let Some(root) = self.flagged_root() else {
            return (Changed::No, None);
        };
        if !self.herdr.ack(&root) {
            return (Changed::No, None);
        }
        self.reconcile_selection();
        // Keyed by path, so acking `/A/proj` leaves `/B/proj` in the window (review (b) F5).
        (
            Changed::Yes,
            Some(Effect::Toast(ToastRequest {
                ready: Vec::new(),
                dropped: vec![root],
            })),
        )
    }

    /// `g`: focus the selected root's agent in herdr. Nothing to do without a pane id.
    fn jump_selected(&mut self) -> (Changed, Option<Effect>) {
        let Some(root) = self.flagged_root() else {
            return (Changed::No, None);
        };
        match self.herdr.flag(&root).and_then(|f| f.pane.clone()) {
            Some(pane) => (Changed::No, Some(Effect::Focus(pane))),
            None => (Changed::No, None),
        }
    }

    /// Fold one piece of herdr news in (deliverables 4, 5, 6, 8).
    pub fn herdr_update(&mut self, update: HerdrUpdate) -> (Changed, Option<Effect>) {
        match update {
            HerdrUpdate::Connected { version, .. } => {
                self.herdr.link = Link::Connected { version };
                (Changed::Yes, None)
            }
            HerdrUpdate::Reconnecting => {
                self.herdr.link = Link::Reconnecting;
                // A link that dropped before its first snapshot never sends a scope.
                self.scope_settled();
                (Changed::Yes, None)
            }
            HerdrUpdate::Standalone { reason } => {
                self.herdr.link = Link::Standalone { reason };
                self.scope_settled();
                (Changed::Yes, None)
            }
            // The picker is live: a pane that appeared or went away while it is open
            // changes the list under the cursor, so the selection is clamped to it.
            HerdrUpdate::Agents(candidates) => {
                if self.herdr.candidates == candidates {
                    return (Changed::No, None);
                }
                self.herdr.candidates = candidates;
                if let Some(picker) = self.picker.take() {
                    let candidates = self.herdr.candidates(&picker.root);
                    // Every candidate gone: there is nobody to send to, so the picker
                    // closes rather than showing an empty list. The flag is already on disk.
                    if candidates.is_empty() {
                        self.set_status(format!("flagged {} · not sent", picker.label));
                    } else {
                        let selected = picker.selected.min(candidates.len() - 1);
                        self.picker = Some(Picker {
                            candidates,
                            selected,
                            ..picker
                        });
                    }
                }
                (Changed::Yes, None)
            }
            HerdrUpdate::Roots(derived) => {
                let delta = self.herdr.apply_roots(derived);
                self.reconcile_selection();
                let request = ToastRequest {
                    ready: delta
                        .opened
                        .iter()
                        .map(|r| (r.clone(), self.root_name(r)))
                        .collect(),
                    dropped: delta.closed.clone(),
                };
                let effect =
                    (self.herdr.toast && !request.is_empty()).then_some(Effect::Toast(request));
                let changed = if delta.changed {
                    Changed::Yes
                } else {
                    Changed::No
                };
                (changed, effect)
            }
            HerdrUpdate::Scope(scope) => {
                // The first verdict lists the roots even when it is the same `None` the
                // view started with.
                let settled = self.scope_settled();
                if self.herdr.scope == scope {
                    return (settled, None);
                }
                tracing::debug!(
                    from = ?self.herdr.scope.as_ref().map(|s| &s.roots),
                    to = ?scope.as_ref().map(|s| &s.roots),
                    "herdr scope"
                );
                self.herdr.scope = scope;
                self.reconcile_selection();
                (Changed::Yes, None)
            }
            HerdrUpdate::Toast(Ok(shown)) if shown.shown => {
                self.set_status("toast shown");
                (Changed::Yes, None)
            }
            // A refusal or a transport error is a debug log in the task, not a banner.
            HerdrUpdate::Toast(_) => (Changed::No, None),
            HerdrUpdate::Focused(Ok(agent)) => {
                self.set_status(format!("focused {agent} in herdr"));
                (Changed::Yes, None)
            }
            HerdrUpdate::Focused(Err(reason)) => {
                self.set_status(format!("jump failed: {reason}"));
                (Changed::Yes, None)
            }
        }
    }

    /// Fold a resolved mouse target in (the loop maps `Press(x, y)` through the `HitMap`).
    pub fn hit(&mut self, target: Target) -> (Changed, Option<Effect>) {
        // The first-launch tour is above every modal, so its rows are resolved first — and
        // a press anywhere else on the overlay is ignored, not passed through to whatever
        // the frame under it happens to be drawing.
        if self.tour.is_some() {
            let Target::TourRow(n) = target else {
                return (Changed::No, None);
            };
            let tour = self.tour.as_mut().expect("checked above");
            if tour.rows() > 0 {
                tour.row = n.min(tour.rows() - 1);
            }
            return self.tour_key(TourKey::Next);
        }
        // A click under any modal is ignored, exactly as it is under the confirm.
        if self.confirm.is_some() || self.note.is_some() || self.picker.is_some() {
            return (Changed::No, None);
        }
        if self.help {
            self.help = false;
            return (Changed::Yes, None);
        }
        let changed = match target {
            // Resolved above, while the overlay is open; once it has closed the rows are
            // gone from the hit map and a stale press on one does nothing.
            Target::TourRow(_) => Changed::No,
            Target::HeaderAcceptAll => return self.handle(Action::AcceptAll),
            Target::RootDot(root) => {
                // The click selects the root exactly as a click on its name does, then
                // acks — key and mouse land on one `App` (the parity test).
                self.select(Some(Selection::Root(root)));
                self.set_focus(Focus::Nav);
                return self.handle(Action::Ack);
            }
            Target::HeaderHerdr => match &self.herdr.link {
                Link::Standalone { reason } if !reason.is_empty() => {
                    let reason = reason.clone();
                    self.set_status(format!("standalone: {reason}"));
                    Changed::Yes
                }
                Link::Connected { version } => {
                    let version = version.clone();
                    self.set_status(format!("herdr {version}"));
                    Changed::Yes
                }
                _ => Changed::No,
            },
            Target::HeaderUpdate => match &self.update {
                Some(version) => {
                    let text = App::update_sentence(version);
                    self.set_status(text);
                    Changed::Yes
                }
                None => Changed::No,
            },
            Target::FileAccept => return self.handle(Action::AcceptFile),
            Target::FileRestore => return self.handle(Action::RestoreFile),
            // The control is only drawn on an expandable row, so the click is the key.
            Target::Expand => return self.handle(Action::Expand),
            Target::HunkAccept(i) => {
                // The cursor first lands on hunk `i` exactly as a click on its header
                // does, then the accept is the one `a` would do there.
                self.hit(Target::DiffHunk(i));
                return self.handle(Action::Accept);
            }
            Target::HunkRestore(i) => {
                // The same two steps as `HunkAccept`, so `u` and a click on `[u restore]`
                // land on one `App` (`input_parity_restore`).
                self.hit(Target::DiffHunk(i));
                return self.handle(Action::Restore);
            }
            Target::HunkFlag(i) => {
                // The same two steps again, so `m` and a click on `[m flag]` open the note
                // modal on one `App` (`input_parity_flag`).
                self.hit(Target::DiffHunk(i));
                return self.handle(Action::Flag);
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
                let offsets = hunk_offsets(self.view_hunks());
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

/// The hunk's header line as both the diff pane and the flag export spell it: `@@ -a,b
/// +c,d @@`, or `mode a → b` for the synthetic mode hunk. One function so a flag quotes
/// exactly the line the reader saw.
pub fn hunk_header(hunk: &Hunk) -> String {
    if hunk.is_mode_change() {
        return format!(
            "mode {} → {}",
            mode_of(&hunk.lines[0].1),
            mode_of(&hunk.lines[1].1)
        );
    }
    format!(
        "@@ -{} +{} @@",
        range_label(hunk.old_range.start, hunk.old_range.len()),
        range_label(hunk.new_range.start, hunk.new_range.len())
    )
}

/// The hunk's body as the export quotes it: every line with its `+`/`-`/space prefix,
/// newline-terminated. The same text the diff pane draws, minus the colour.
pub fn hunk_body(hunk: &Hunk) -> String {
    let mut out = String::new();
    for (tag, bytes) in &hunk.lines {
        out.push(match tag {
            Tag::Context => ' ',
            Tag::Insert => '+',
            Tag::Delete => '-',
        });
        out.push_str(&hunk_line_text(bytes));
        out.push('\n');
    }
    out
}

pub fn mode_of(line: &[u8]) -> String {
    String::from_utf8_lossy(line)
        .trim()
        .trim_start_matches("mode ")
        .to_owned()
}

/// git's `start[,len]`: 1-based start for a non-empty range, the preceding line for an
/// empty one, and `,len` omitted when it is 1 (as `git diff` prints it).
pub fn range_label(start: usize, len: usize) -> String {
    match len {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{len}", start + 1),
    }
}

/// One quoted diff line: lossy UTF-8 with its terminator trimmed, so a CRLF file does not
/// put a stray `^M` in the export. Tabs stay tabs here — the diff pane expands them to
/// four spaces for the screen, but a flag quotes the file's own bytes.
fn hunk_line_text(bytes: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(bytes).into_owned();
    while s.ends_with('\n') || s.ends_with('\r') {
        s.pop();
    }
    s
}

/// Lines a hunk occupies in the diff: its header plus its lines; a mode-change hunk is the
/// single line `mode a → b`.
pub fn hunk_height(hunk: &Hunk) -> usize {
    if hunk.is_mode_change() {
        1
    } else {
        1 + hunk.lines.len()
    }
}

/// Diff lines hunk `i` occupies *with* its separator: exactly one blank line follows every
/// hunk but the last, so consecutive hunks are told apart on screen. The last hunk has none
/// (and neither does the first get one before it), so a one-hunk row is unchanged.
pub fn hunk_block(hunks: &[Hunk], i: usize) -> usize {
    hunk_height(&hunks[i]) + usize::from(i + 1 < hunks.len())
}

/// First diff line of each hunk (its header), counting the blank separator lines.
pub fn hunk_offsets(hunks: &[Hunk]) -> Vec<usize> {
    let mut out = Vec::with_capacity(hunks.len());
    let mut at = 0;
    for i in 0..hunks.len() {
        out.push(at);
        at += hunk_block(hunks, i);
    }
    out
}

/// Total diff lines of a hunk list, counting the blank separator lines.
pub fn diff_lines(hunks: &[Hunk]) -> usize {
    (0..hunks.len()).map(|i| hunk_block(hunks, i)).sum()
}

/// Diff line `i` of `hunks` exactly as the pane shows it, or `None` past the end: the
/// header for a hunk's first line, `+`/`-`/space and the line's own text for its body, and
/// the empty string for the blank separator between two hunks.
///
/// This is what a copy puts on the clipboard, and it is the same text
/// [`hunk_header`]/[`hunk_body`] give a flag export — tabs stay tabs, because the paste
/// target wants the file's own bytes; only the *screen* expands them.
pub fn diff_line_text(hunks: &[Hunk], i: usize) -> Option<String> {
    let offsets = hunk_offsets(hunks);
    let h = offsets.partition_point(|&o| o <= i).checked_sub(1)?;
    let within = i - offsets[h];
    let hunk = &hunks[h];
    if within == 0 {
        return Some(hunk_header(hunk));
    }
    if within >= hunk_height(hunk) {
        // The blank separator line: on screen it spaces two hunks apart, and in a copy it
        // does the same.
        return (within < hunk_block(hunks, h)).then(String::new);
    }
    let (tag, bytes) = &hunk.lines[within - 1];
    let prefix = match tag {
        Tag::Context => ' ',
        Tag::Insert => '+',
        Tag::Delete => '-',
    };
    Some(format!("{prefix}{}", hunk_line_text(bytes)))
}

/// Total diff lines of a row's own hunks. The diff pane may be showing an expansion
/// instead ([`App::view_hunks`]); this is the row's shape, not the screen's.
pub fn diff_len(row: &Row) -> usize {
    diff_lines(&row.hunks)
}

pub fn short(oid: &Oid) -> String {
    oid.as_str().chars().take(7).collect()
}

/// `1 file`, `2 files`.
/// `1 file`, `2 files`, `1,234 files`: every count on screen goes through the engine's
/// [`with_thousands`], the same formatter the row-cap notice uses.
pub fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{} {noun}s", with_thousands(n))
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

/// The whole-row restore scope for `row`, with the two facts the confirm wording turns on.
fn restore_file_of(root: PathBuf, row: &Row) -> RestoreScope {
    RestoreScope::File {
        root,
        path: row.path.clone(),
        deleted: row.change == Change::Deleted,
        added: row.change == Change::Added,
        hunks: row.hunks.len(),
    }
}

/// The status line for a restore that went through.
fn restored_text(scope: &RestoreScope) -> String {
    let lossy = |p: &[u8]| String::from_utf8_lossy(p).into_owned();
    match scope {
        // 1-based, like the diff's own `hunk n of m`: the reader counts from one.
        RestoreScope::Hunk { path, index, .. } => {
            format!("restored {} hunk {}", lossy(path), index + 1)
        }
        RestoreScope::File { path, added, .. } if *added => {
            format!("removed {} (added since baseline)", lossy(path))
        }
        RestoreScope::File { path, .. } => format!("restored {}", lossy(path)),
    }
}

/// The confirm modal's first row for a restore, and the only place its wording lives.
pub fn restore_question(scope: &RestoreScope) -> String {
    let lossy = |p: &[u8]| String::from_utf8_lossy(p).into_owned();
    match scope {
        RestoreScope::Hunk { path, index, .. } => {
            format!("Restore {} hunk {}?", lossy(path), index + 1)
        }
        RestoreScope::File { path, added, .. } if *added => {
            format!("Delete {}? (added since baseline)", lossy(path))
        }
        RestoreScope::File { path, deleted, .. } if *deleted => {
            format!("Restore {}? (deleted)", lossy(path))
        }
        RestoreScope::File { path, hunks, .. } => {
            format!("Restore {} · {}?", lossy(path), plural(*hunks, "hunk"))
        }
    }
}

/// The status text for refusals: one or two joined by ` · `, more as the first plus
/// ` (+N more)`.
pub fn refusal_text(refusals: &[String]) -> String {
    match refusals {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} · {b}"),
        [first, rest @ ..] => format!("{first} (+{} more)", with_thousands(rest.len())),
    }
}

pub fn basename(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

/// `path` with the home folder written `~`, which is how the header shows where a root is.
///
/// A prefix match on whole components only: `/home/user2` is not inside `/home/user`, and
/// the home itself is `~`. With no home known, or a home of `/`, the path is shown as it
/// is — a `~` that stood for the whole filesystem would say nothing.
pub fn collapse_home(path: &Path, home: Option<&Path>) -> String {
    let shown = path.to_string_lossy().into_owned();
    let Some(home) = home else { return shown };
    let home = home.to_string_lossy();
    if home.is_empty() || home == "/" {
        return shown;
    }
    if shown == home {
        return "~".to_owned();
    }
    match shown.strip_prefix(&format!("{home}/")) {
        Some(rest) => format!("~/{rest}"),
        None => shown,
    }
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
            path_shown: format!("~/W/{name}"),
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
    pub fn accepted_ok(name: &str, seq: u64, pile: Pile) -> (PathBuf, AcceptResult) {
        (
            root(name),
            Ok(Accepted {
                outcome: lastcall_engine::ops::Outcome::default(),
                seq,
                pile,
            }),
        )
    }

    /// An engine answer for an `Effect::Undo`: a clean outcome, the paths it put back, and
    /// `pile` as the rescan.
    pub fn undone_ok(name: &str, seq: u64, paths: &[&str], pile: Pile) -> (PathBuf, UndoResult) {
        (
            root(name),
            Ok(Undone {
                outcome: lastcall_engine::ops::Outcome::default(),
                op: Some(lastcall_engine::ledger::UndoOp::AcceptFile),
                paths: paths.iter().map(|p| (*p).to_owned()).collect(),
                seq,
                pile,
            }),
        )
    }

    /// An engine answer for an `Effect::Snooze`: `until` is `None` for a wake.
    pub fn snoozed_ok(
        name: &str,
        seq: u64,
        until: Option<&str>,
        pile: Pile,
    ) -> (PathBuf, SnoozeResult) {
        (
            root(name),
            Ok(Snoozed {
                outcome: lastcall_engine::ops::Outcome::default(),
                until: until.map(str::to_owned),
                seq,
                pile,
            }),
        )
    }

    /// `pile` with a snooze deadline on it, as the engine stamps one at scan time.
    pub fn snoozed_pile(mut pile: Pile, until: &str) -> Pile {
        pile.snoozed_until = Some(until.to_owned());
        pile
    }

    /// `pile` with an undo stack `n` deep, as the engine stamps one at scan time.
    pub fn undo_pile(mut pile: Pile, n: usize) -> Pile {
        pile.undo = n;
        pile
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

    /// alpha's pile with `f1` given one hunk whose body is the `lines` named, all context
    /// lines. The wrap tests need bodies whose widths they chose.
    pub fn alpha_texts(lines: &[&str]) -> Pile {
        let mut p = pile("alpha");
        let mut h = p.rows[0].hunks[0].clone();
        h.index = 0;
        h.lines = lines
            .iter()
            .map(|t| {
                (
                    lastcall_engine::hunks::Tag::Context,
                    format!("{t}\n").into_bytes(),
                )
            })
            .collect();
        p.rows[0].hunks = vec![h];
        p
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

    /// alpha's pile with `f1` given one hunk per zero-based new-side `(start, end)` range,
    /// all cloned from `f1`'s own (whose first line is a deletion, so
    /// [`Hunk::editor_line`] of each is `start + 1`).
    ///
    /// The recorded fixture's `f1` has a single hunk at `0..4`, which is not enough to say
    /// anything about *which* lines the inline editor marks or tints: the deliverable-8
    /// tests need hunks at line numbers they can tell apart.
    pub fn alpha_ranges(ranges: &[(usize, usize)]) -> Pile {
        let mut p = pile("alpha");
        let first = p.rows[0].hunks[0].clone();
        p.rows[0].hunks = ranges
            .iter()
            .enumerate()
            .map(|(i, (start, end))| {
                let mut h = first.clone();
                h.index = i;
                h.new_range = *start..*end;
                h.old_range = *start..*end;
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

    /// alpha's pile with `f1` collapsed as `kind` and its hunks cleared, exactly as the
    /// scan leaves a collapsed row (Phase 6 deliverable 4).
    pub fn alpha_collapsed(kind: Collapsed) -> Pile {
        let mut p = pile("alpha");
        p.rows[0].collapsed = Some(kind);
        p.rows[0].hunks.clear();
        p
    }

    /// An expansion answer of `n` one-line hunks, `omitted` body lines short of the whole.
    pub fn expansion_of(n: usize, omitted: usize) -> Expanded {
        let template = pile("alpha").rows[0].hunks[0].clone();
        Expanded {
            hunks: (0..n)
                .map(|i| {
                    let mut h = template.clone();
                    h.index = i;
                    h
                })
                .collect(),
            omitted_lines: omitted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testfix::*;
    use super::*;
    use crate::tui::input::{EditKey, editor_action, note_action};
    use crate::tui::textbuf::Pos;
    use crossterm::event::{Event, KeyCode, KeyModifiers};
    use lastcall_engine::roots::RootsChanged;
    /// Phase 6 deliverable 4: `e` is silent where it has nothing to do — a binary row
    /// (never expandable), a row that is not collapsed at all, a group, and a row already
    /// expanded. No effect means no engine work; `Changed::No` means no draw.
    /// R5: the header's path line writes the home folder `~`, and a folder that merely
    /// starts with the same letters is not inside it.
    #[test]
    fn app_collapse_home_matches_whole_components_only() {
        let home = Path::new("/home/u");
        assert_eq!(
            collapse_home(Path::new("/home/u/w/notes"), Some(home)),
            "~/w/notes"
        );
        assert_eq!(collapse_home(Path::new("/home/u"), Some(home)), "~");
        assert_eq!(
            collapse_home(Path::new("/home/u2/w"), Some(home)),
            "/home/u2/w",
            "a different home of the same prefix is not inside this one"
        );
        assert_eq!(
            collapse_home(Path::new("/srv/w"), Some(home)),
            "/srv/w",
            "outside the home: shown as it is"
        );
        assert_eq!(
            collapse_home(Path::new("/srv/w"), None),
            "/srv/w",
            "no home known: shown as it is"
        );
        assert_eq!(
            collapse_home(Path::new("/srv/w"), Some(Path::new("/"))),
            "/srv/w",
            "a home of `/` would make `~` mean nothing"
        );
    }

    #[test]
    fn app_expand_is_a_no_op_off_a_collapsed_row() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        // A plain hunk row.
        app.select(Some(row("alpha", "f1")));
        assert_eq!(app.handle(Action::Expand), (Changed::No, None));
        // A binary row.
        app.apply(pile_event_seq(
            "alpha",
            1,
            alpha_collapsed(Collapsed::Binary),
        ));
        app.select(Some(row("alpha", "f1")));
        assert_eq!(app.handle(Action::Expand), (Changed::No, None));
        assert!(app.expanded.is_none());
        // A group entry.
        app.select(Some(Selection::Group(root("beta"), Annotation::Upstream)));
        assert_eq!(app.handle(Action::Expand), (Changed::No, None));
    }

    /// `e` on a `Glob`/`Size` row asks the engine once, with the row itself so the
    /// expansion is computed from the oids the screen showed. The answer lives beside the
    /// pile: the row keeps no hunks, so `a` and `A` are still one whole-row accept.
    #[test]
    fn app_expand_holds_the_hunks_beside_the_row_and_accept_stays_whole_row() {
        for kind in [Collapsed::Glob, Collapsed::Size] {
            let mut app = three_roots();
            app.handle(Action::Resize(100, 30));
            app.apply(pile_event_seq("alpha", 1, alpha_collapsed(kind)));
            app.select(Some(row("alpha", "f1")));
            let (changed, effect) = app.handle(Action::Expand);
            assert_eq!(changed, Changed::No, "asking draws nothing");
            let Some(Effect::Expand(asked_root, asked_row)) = effect else {
                panic!("an expand effect: {effect:?}");
            };
            assert_eq!(asked_root, root("alpha"));
            assert_eq!(asked_row.path, b"f1");
            assert_eq!(asked_row.collapsed, Some(kind));

            let view = expansion_of(3, 0);
            assert_eq!(
                app.set_expanded(root("alpha"), &asked_row, view.clone()),
                Changed::Yes
            );
            assert_eq!(app.view_hunks().len(), 3, "the diff pane shows the hunks");
            assert!(
                app.selected_row().unwrap().hunks.is_empty(),
                "never written back onto the row"
            );
            assert!(
                matches!(
                    app.accept_scope(),
                    Some(AcceptAnswer::Take(AcceptScope::File { .. }))
                ),
                "a collapsed row is one accept however much is on screen: {:?}",
                app.accept_scope()
            );
            // Asking again while the same expansion is held is a second no-op.
            assert_eq!(app.handle(Action::Expand), (Changed::No, None));
            // The hunk cursor is bounded by the expansion, not by the row's empty list.
            assert_eq!(app.handle(Action::HunkNext).0, Changed::Yes);
            assert_eq!(app.diff.hunk, 1);
        }
    }

    /// The expansion is a view of one row at one moment: it goes when the selection leaves
    /// the row, and when a newer pile moves that row's oids.
    #[test]
    fn app_expansion_is_dropped_by_a_new_selection_or_new_oids() {
        let collapsed = alpha_collapsed(Collapsed::Glob);
        let expand = |app: &mut App| {
            app.select(Some(row("alpha", "f1")));
            let asked = app.selected_row().unwrap().clone();
            assert_eq!(
                app.set_expanded(root("alpha"), &asked, expansion_of(2, 0)),
                Changed::Yes
            );
        };

        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.apply(pile_event_seq("alpha", 1, collapsed.clone()));
        expand(&mut app);
        app.select(Some(row("alpha", "f2")));
        assert!(app.expanded.is_none(), "the selection left the row");

        // A pile that repeats the same oids keeps it: nothing the reader is looking at moved.
        expand(&mut app);
        let mut same = collapsed.clone();
        same.notices.push("alpha: something else".to_owned());
        app.apply(pile_event_seq("alpha", 2, same));
        assert!(app.expanded.is_some(), "same oids, same expansion");

        // A pile whose row has a new current oid drops it.
        let mut moved = collapsed.clone();
        moved.rows[0].current.as_mut().unwrap().oid =
            Oid::parse(&"a".repeat(40)).expect("a well-formed oid");
        app.apply(pile_event_seq("alpha", 3, moved));
        assert!(app.expanded.is_none(), "the row's content moved under it");

        // So does the row disappearing entirely.
        expand(&mut app);
        app.apply(pile_event_seq("alpha", 4, without(collapsed, &["f1"])));
        assert!(app.expanded.is_none(), "the row is gone");
    }

    /// An answer that arrives after the reader moved on is dropped, not shown: it is a
    /// diff of something no longer selected.
    #[test]
    fn app_expansion_answer_for_another_row_is_dropped() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.apply(pile_event_seq("alpha", 1, alpha_collapsed(Collapsed::Glob)));
        let asked = app.roots[&root("alpha")].row(b"f1").unwrap().clone();
        app.select(Some(row("alpha", "f2")));
        assert_eq!(
            app.set_expanded(root("alpha"), &asked, expansion_of(2, 0)),
            Changed::No
        );
        assert!(app.expanded.is_none());
    }

    /// Verifier (b) F1: an answer computed for the oids `e` was pressed on is dropped when a
    /// pile has moved the row's oids meanwhile — the row is still selected, but the hunks
    /// describe a delta the counts on screen no longer do. Storing it under the row's new
    /// oids would have kept it through every later identical pile.
    #[test]
    fn app_expansion_answer_for_moved_oids_is_dropped() {
        let collapsed = alpha_collapsed(Collapsed::Glob);
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.apply(pile_event_seq("alpha", 1, collapsed.clone()));
        app.select(Some(row("alpha", "f1")));
        let (_, effect) = app.handle(Action::Expand);
        let Some(Effect::Expand(_, asked)) = effect else {
            panic!("an expand effect: {effect:?}");
        };

        // The file is rewritten again before `hunks_of` answers.
        let mut moved = collapsed;
        moved.rows[0].current.as_mut().unwrap().oid =
            Oid::parse(&"c".repeat(40)).expect("a well-formed oid");
        app.apply(pile_event_seq("alpha", 2, moved));
        assert_eq!(app.selection, Some(row("alpha", "f1")), "still selected");

        assert_eq!(
            app.set_expanded(root("alpha"), &asked, expansion_of(2, 0)),
            Changed::No,
            "a diff of oids the screen no longer shows"
        );
        assert!(app.expanded.is_none());
        assert!(
            app.view_hunks().is_empty(),
            "the collapsed placeholder stays"
        );

        // A fresh request answers against the new oids and is shown.
        let (_, effect) = app.handle(Action::Expand);
        let Some(Effect::Expand(_, again)) = effect else {
            panic!("an expand effect: {effect:?}");
        };
        assert_ne!(
            again.current, asked.current,
            "the new request carries the new oids"
        );
        assert_eq!(
            app.set_expanded(root("alpha"), &again, expansion_of(2, 0)),
            Changed::Yes
        );
    }

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
        // `A` is the whole file from the nav pane too (the ruling: it is the only key that
        // takes a whole file).
        let mut nav = three_roots();
        nav.select(Some(row("alpha", "f1")));
        assert_eq!(nav.focus, Focus::Nav);
        assert_eq!(nav.handle(Action::AcceptFile).1, key_effect.1);
    }

    /// The ruling: on a file row `a` takes ONE hunk — the one under the diff cursor —
    /// whichever pane has focus, and the request is the diff-focused one exactly.
    #[test]
    fn app_accept_from_the_nav_pane_takes_one_hunk_not_the_file() {
        let mut base = three_roots();
        base.apply(pile_event("alpha", alpha_hunks(3)));
        base.select(Some(row("alpha", "f1")));

        let mut nav = base.clone();
        assert_eq!(nav.focus, Focus::Nav);
        nav.handle(Action::HunkNext);
        assert_eq!(nav.diff.hunk, 1, "n moves the diff cursor from either pane");
        let nav_scope = nav.accept_scope();
        let nav_effect = nav.handle(Action::Accept);

        let mut diff = base.clone();
        diff.handle(Action::Open);
        assert_eq!(diff.effective_focus(), Focus::Diff);
        diff.handle(Action::HunkNext);
        let diff_effect = diff.handle(Action::Accept);

        let held = base.roots[&root("alpha")].row(b"f1").unwrap();
        assert_eq!(
            nav_scope,
            Some(AcceptAnswer::Take(AcceptScope::Hunk {
                root: root("alpha"),
                path: b"f1".to_vec(),
                index: 1,
                hunks: 3,
            })),
            "not AcceptScope::File"
        );
        assert_eq!(
            requests(nav_effect.1.clone()),
            vec![(
                root("alpha"),
                AcceptRequest::Hunk {
                    rendered: Rendered::of(held),
                    hunks: held.hunks.clone(),
                    index: 1,
                }
            )]
        );
        assert_eq!(
            nav_effect.1, diff_effect.1,
            "the same request from either pane"
        );
        assert_eq!(nav.accepting, diff.accepting);

        // `A` from the nav pane is still the whole file.
        let mut whole = base.clone();
        assert_eq!(
            requests(whole.handle(Action::AcceptFile).1),
            vec![(root("alpha"), AcceptRequest::File(Rendered::of(held)))]
        );

        // A root entry's `a` refuses since Amendment v1.11 and names `A`; `A` is the fold.
        let mut fold = base;
        fold.select(Some(Selection::Root(root("alpha"))));
        assert_eq!(
            fold.accept_scope(),
            Some(AcceptAnswer::Refuse("A accepts all in alpha".to_owned()))
        );
        assert_eq!(
            fold.accept_file_scope(),
            Some(AcceptScope::Root(root("alpha")))
        );
    }

    /// The carve-out: a file row with no hunks to point at (binary, collapsed, deleted,
    /// unreadable) keeps `a` = the whole row, in either pane.
    #[test]
    fn app_accept_on_a_hunkless_row_is_still_the_whole_row() {
        let mut hunkless = pile("alpha");
        hunkless.rows[0].hunks.clear();
        let mut app = three_roots();
        app.apply(pile_event("alpha", hunkless));
        app.select(Some(row("alpha", "f1")));
        let held = app.roots[&root("alpha")].row(b"f1").unwrap().clone();
        assert!(held.hunks.is_empty());
        let expect = Some(AcceptAnswer::Take(AcceptScope::File {
            root: root("alpha"),
            path: b"f1".to_vec(),
            deleted: held.change == Change::Deleted,
        }));
        assert_eq!(app.accept_scope(), expect, "nav pane");
        app.handle(Action::Open);
        assert_eq!(app.effective_focus(), Focus::Diff);
        assert_eq!(app.accept_scope(), expect, "diff pane");
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
            vec![(root("alpha"), 3), (root("beta"), 2), (root("notes"), 1)]
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

    /// Amendment v1.11: the whole-repository fold and the whole-group fold are `A`'s, not
    /// `a`'s. What they fold, and where the cursor lands afterwards, is unchanged.
    #[test]
    fn app_accept_file_on_root_entry_folds_the_held_pile_and_on_group_the_group() {
        let mut app = three_roots();
        app.select(Some(Selection::Root(root("alpha"))));
        assert_eq!(
            requests(app.handle(Action::AcceptFile).1),
            vec![(root("alpha"), AcceptRequest::All(pile("alpha")))]
        );
        assert_eq!(
            app.accepted(vec![accepted_ok("alpha", 2, Pile::default())]),
            Changed::Yes
        );
        assert_eq!(status(&app), "accepted 3 files in alpha");
        assert_eq!(
            app.selection,
            Some(Selection::Root(root("alpha"))),
            "the repo stays on the nav and keeps the cursor (§6.7, Amendment v1.9)"
        );
        assert!(!app.roots[&root("alpha")].listed(), "no rows left");
        assert!(app.nav_entries().contains(&Selection::Root(root("alpha"))));

        app.select(Some(Selection::Group(root("beta"), Annotation::Upstream)));
        let beta = pile("beta");
        let group = beta.groups().into_iter().next().unwrap();
        let rendered: Vec<Rendered> = group
            .paths
            .iter()
            .map(|p| Rendered::of(beta.row(p).unwrap()))
            .collect();
        assert_eq!(
            requests(app.handle(Action::AcceptFile).1),
            vec![(
                root("beta"),
                AcceptRequest::Group {
                    rows: rendered,
                    rendered_on: beta.seen_branch.clone(),
                }
            )]
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

    /// Amendment v1.11, the ruling of 2026-09-14: a lowercase `a` on a repository row no
    /// longer folds the repository. It accepts nothing, and the status names the key that
    /// does — which is the whole point, because a repository of ten files or fewer used to
    /// vanish with no confirm at all.
    #[test]
    fn app_accept_on_a_repo_row_refuses_and_names_the_accept_file_key() {
        let mut app = three_roots();
        app.select(Some(Selection::Root(root("alpha"))));
        let before = app.roots[&root("alpha")].clone();

        assert_eq!(app.handle(Action::Accept), (Changed::Yes, None));
        assert_eq!(status(&app), "A accepts all in alpha");
        assert_eq!(app.accepting, None, "nothing was sent to the engine");
        assert_eq!(
            app.roots[&root("alpha")],
            before,
            "the held pile is untouched"
        );

        // `A` is the fold, and above `CONFIRM_ABOVE` files it asks first.
        assert_eq!(
            app.accept_file_scope(),
            Some(AcceptScope::Root(root("alpha")))
        );
        let mut big = three_roots();
        big.apply(pile_event_seq("alpha", 1, rows_n(11, 0, 0)));
        big.select(Some(Selection::Root(root("alpha"))));
        assert_eq!(big.handle(Action::AcceptFile), (Changed::Yes, None));
        assert_eq!(
            big.confirm,
            Some(Confirm {
                scope: ConfirmScope::Accept(AcceptScope::Root(root("alpha")))
            }),
            "eleven files ask"
        );
    }

    /// A group is several files too, so `a` refuses there on the same terms and `A` folds
    /// it. The refusal names the group rather than a repository.
    #[test]
    fn app_accept_on_a_group_refuses_and_accept_file_folds_it() {
        let mut app = three_roots();
        app.select(Some(Selection::Group(root("beta"), Annotation::Upstream)));
        let before = app.roots[&root("beta")].clone();

        assert_eq!(
            app.accept_scope(),
            Some(AcceptAnswer::Refuse("A accepts the group".to_owned()))
        );
        assert_eq!(app.handle(Action::Accept), (Changed::Yes, None));
        assert_eq!(status(&app), "A accepts the group");
        assert_eq!(app.accepting, None);
        assert_eq!(app.roots[&root("beta")], before);

        assert_eq!(
            app.accept_file_scope(),
            Some(AcceptScope::Group {
                root: root("beta"),
                kind: Annotation::Upstream,
            })
        );
    }

    /// An **empty** repository row has nothing to refuse over: both keys read `nothing to
    /// accept`, the answer `a` gave there before the ruling (verifier (a) F2's row).
    #[test]
    fn app_accept_and_accept_file_on_an_empty_repo_row_read_nothing_to_accept() {
        let mut app = three_roots();
        app.apply(pile_event_seq("alpha", 1, Pile::default()));
        app.select(Some(Selection::Root(root("alpha"))));
        assert!(app.roots[&root("alpha")].rows().is_empty());

        assert_eq!(
            app.accept_scope(),
            Some(AcceptAnswer::Take(AcceptScope::Root(root("alpha")))),
            "no refusal on an empty row"
        );
        assert_eq!(app.handle(Action::Accept), (Changed::Yes, None));
        assert_eq!(status(&app), NOTHING_TO_ACCEPT);
        assert_eq!(app.handle(Action::AcceptFile), (Changed::Yes, None));
        assert_eq!(status(&app), NOTHING_TO_ACCEPT);
        assert_eq!(app.accepting, None);
    }

    /// The refusals are spelled from the **effective** keymap, the way the help overlay
    /// spells a key, so a reader who rebound `accept_file` is sent to their own key.
    #[test]
    fn app_accept_refusals_spell_a_rebound_accept_file_key() {
        let mut app = three_roots();
        for (name, specs) in &mut app.keymap {
            if name == "accept_file" {
                *specs = vec!["ctrl-w".to_owned()];
            }
        }
        app.select(Some(Selection::Root(root("alpha"))));
        assert_eq!(
            app.accept_scope(),
            Some(AcceptAnswer::Refuse(
                "Ctrl-W accepts all in alpha".to_owned()
            ))
        );
        app.select(Some(Selection::Group(root("beta"), Annotation::Upstream)));
        assert_eq!(
            app.accept_scope(),
            Some(AcceptAnswer::Refuse("Ctrl-W accepts the group".to_owned()))
        );
    }

    // ---- the post-$EDITOR blessing (Phase 8 deliverable 3) -----------------------------

    /// The row `f1` was rendered from, and the same row with the editor's bytes on it.
    fn editor_pair(app: &App) -> (Rendered, Current) {
        let rendered = Rendered::of(app.roots[&root("alpha")].row(b"f1").expect("f1"));
        let live = Current::Present {
            oid: Oid::parse(&"e".repeat(40)).expect("a well-formed oid"),
            mode: rendered.mode.expect("f1 is not a deletion"),
        };
        (rendered, live)
    }

    /// Deliverable 3 / ruling P1: a file the editor session changed is never blessed
    /// silently. `Enter` sends the accept **with the live oid** — not the one on the held
    /// row — and `Esc` sends nothing and says the row is still pending.
    #[test]
    fn app_editor_return_asks_before_blessing_a_changed_file() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        let (rendered, live) = editor_pair(&app);
        let Current::Present { oid: live_oid, .. } = live.clone() else {
            unreachable!("built as Present");
        };

        // The return itself writes nothing: it opens the question.
        let (changed, effect) = app.editor_returned(root("alpha"), rendered.clone(), live.clone());
        assert_eq!(changed, Changed::Yes);
        assert_eq!(effect, None, "the confirm is the only thing that happens");
        assert_eq!(
            app.confirm_bless(),
            Some(&b"f1"[..]),
            "the confirm names the path the editor had open"
        );
        assert!(app.accepting.is_none(), "nothing is in flight yet");

        // `y` / Enter: the request carries what is on disk now, and the held row's own oid
        // is nowhere in it — that is the whole of the blessing.
        let (changed, effect) = app.handle(Action::Confirm);
        assert_eq!(changed, Changed::Yes);
        assert!(app.confirm.is_none());
        let reqs = requests(effect);
        assert_eq!(
            reqs,
            vec![(
                root("alpha"),
                AcceptRequest::File(Rendered {
                    oid: Some(live_oid.clone()),
                    ..rendered.clone()
                })
            )]
        );
        assert_ne!(rendered.oid, Some(live_oid), "the live oid is a new one");
        assert_eq!(status(&app), "accepting…");

        // The answer names the session, not an accept: `reviewed`, and the row is gone.
        app.accepted(vec![accepted_ok(
            "alpha",
            4,
            without(pile("alpha"), &["f1"]),
        )]);
        assert_eq!(status(&app), "reviewed f1");
        assert!(app.roots[&root("alpha")].row(b"f1").is_none());

        // Esc: no effect at all, and the row is still there to review the ordinary way.
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        let (rendered, live) = editor_pair(&app);
        app.editor_returned(root("alpha"), rendered, live);
        let (changed, effect) = app.handle(Action::Cancel);
        assert_eq!(changed, Changed::Yes);
        assert_eq!(effect, None, "a declined blessing writes nothing");
        assert!(app.confirm.is_none());
        assert!(app.accepting.is_none());
        assert_eq!(status(&app), "f1 left pending");
        assert!(app.roots[&root("alpha")].row(b"f1").is_some());
    }

    /// A look-and-quit — and every non-waiting editor, which returns before the user has
    /// saved — leaves the file byte-identical: no question, no write, one word.
    #[test]
    fn app_editor_return_skips_an_unchanged_file() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        let rendered = Rendered::of(app.roots[&root("alpha")].row(b"f1").expect("f1"));
        let live = Current::Present {
            oid: rendered.oid.clone().expect("f1 is not a deletion"),
            mode: rendered.mode.expect("f1 is not a deletion"),
        };
        let (changed, effect) = app.editor_returned(root("alpha"), rendered.clone(), live);
        assert_eq!(changed, Changed::Yes);
        assert_eq!(effect, None);
        assert!(app.confirm.is_none(), "nothing to ask about");
        assert_eq!(status(&app), "no change");

        // The mode alone is enough to make it a change: `chmod +x` inside the editor is a
        // delta the ledger has to record, so it asks.
        let live = Current::Present {
            oid: rendered.oid.clone().expect("f1 is not a deletion"),
            mode: Mode::Executable,
        };
        app.editor_returned(root("alpha"), rendered, live);
        assert_eq!(app.confirm_bless(), Some(&b"f1"[..]));
    }

    /// Nothing on disk to bless: the file is gone, or it is not a regular file. Each says
    /// which, and each leaves the row exactly as it was.
    #[test]
    fn app_editor_return_leaves_a_deleted_file_pending() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        let rendered = Rendered::of(app.roots[&root("alpha")].row(b"f1").expect("f1"));
        let held = app.clone();

        for (live, expect) in [
            (Current::Absent, "f1: deleted on return; left pending"),
            (
                Current::Present {
                    oid: Oid::parse(&"e".repeat(40)).expect("a well-formed oid"),
                    mode: Mode::Symlink,
                },
                "f1: not a regular file on return; left pending",
            ),
            (
                Current::Unhashable("typechange: a directory where a file was".into()),
                "f1: typechange: a directory where a file was on return; left pending",
            ),
        ] {
            let (changed, effect) = app.editor_returned(root("alpha"), rendered.clone(), live);
            assert_eq!(changed, Changed::Yes);
            assert_eq!(effect, None, "nothing is written");
            assert!(app.confirm.is_none(), "and nothing is asked");
            assert_eq!(status(&app), expect);
            assert!(app.accepting.is_none());
            assert_eq!(
                app.roots[&root("alpha")].row(b"f1"),
                held.roots[&root("alpha")].row(b"f1"),
                "the row is untouched"
            );
        }
    }

    /// Verifier (a) F1: the return arrives on a channel, so a question can already be on
    /// screen when it lands. Replacing it would mean the next `y` answers a question the
    /// user never read — so the blessing is not asked at all and the row stays pending with
    /// the editor's delta on it.
    #[test]
    fn app_editor_return_leaves_the_row_pending_when_a_modal_is_open() {
        // A confirm: accept-all over twelve files, waiting for its answer.
        let mut app = three_roots();
        app.apply(pile_event("alpha", rows_n(12, 0, 0)));
        app.select(Some(Selection::Root(root("alpha"))));
        app.handle(Action::AcceptFile);
        let asked = app.confirm.clone();
        assert!(asked.is_some(), "the accept-all question is up");

        let rendered = Rendered::of(app.roots[&root("alpha")].row(b"p00").expect("p00"));
        let live = Current::Present {
            oid: Oid::parse(&"e".repeat(40)).expect("a well-formed oid"),
            mode: rendered.mode.expect("not a deletion"),
        };
        let (changed, effect) = app.editor_returned(root("alpha"), rendered.clone(), live.clone());
        assert_eq!(changed, Changed::Yes, "only the status moved");
        assert_eq!(effect, None);
        assert_eq!(app.confirm, asked, "the accept-all question is untouched");
        assert_eq!(app.confirm_bless(), None, "and it is not a blessing");
        assert_eq!(status(&app), "p00: changed on return; left pending");
        assert!(
            app.roots[&root("alpha")].row(b"p00").is_some(),
            "the row is still there to review the ordinary way"
        );

        // The note modal: the same answer.
        let mut app = note_open();
        let rendered = Rendered::of(app.roots[&root("alpha")].row(b"f1").expect("f1"));
        let live = Current::Present {
            oid: Oid::parse(&"e".repeat(40)).expect("a well-formed oid"),
            mode: rendered.mode.expect("not a deletion"),
        };
        app.editor_returned(root("alpha"), rendered.clone(), live.clone());
        assert!(app.note.is_some(), "the note is still being typed");
        assert!(app.confirm.is_none(), "nothing was asked over it");
        assert_eq!(status(&app), "f1: changed on return; left pending");

        // The agent picker: likewise.
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.handle(agents_of(
            "alpha",
            vec![
                agent("w1:p1", "claude", "lastcall"),
                agent("w2:p3", "codex", "spike"),
            ],
        ));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Flag);
        type_note(&mut app, "look at this");
        app.handle(Action::Note(NoteKey::Send));
        app.flagged(
            root("alpha"),
            flag_of("f1"),
            flagged_ok("EXPORT", 1, pile("alpha")),
        );
        assert!(app.picker.is_some(), "the picker is up");
        app.editor_returned(root("alpha"), rendered, live);
        assert!(app.picker.is_some(), "and it stays up");
        assert!(app.confirm.is_none());
        assert_eq!(status(&app), "f1: changed on return; left pending");
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
        assert_eq!(app.handle(Action::AcceptFile), (Changed::Yes, None));
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
                scope: ConfirmScope::Accept(AcceptScope::All)
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
        app.handle(Action::AcceptFile);
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
                app.handle(action.clone()),
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
        assert_eq!(status(&app), "accepted f1 · 2 hunks left");
        assert_eq!(app.selection, Some(row("alpha", "f1")));
        assert_eq!(app.diff.hunk, 1, "the next hunk slid into the cursor");
        assert_eq!(app.diff.scroll, hunk_offsets(&two.rows[0].hunks)[1]);
        assert_eq!(app.focus, Focus::Diff);
        assert_eq!(app.accepting, None);

        app.handle(Action::Accept);
        let one = alpha_hunks(1);
        app.accepted(vec![accepted_ok("alpha", 3, one)]);
        assert_eq!(status(&app), "accepted f1 · 1 hunk left");
        assert_eq!(app.diff, DiffCursor { hunk: 0, scroll: 0 }, "clamped");

        app.handle(Action::Accept);
        app.accepted(vec![accepted_ok(
            "alpha",
            4,
            without(pile("alpha"), &["f1"]),
        )]);
        assert_eq!(status(&app), "accepted f1 · file complete");
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
    fn app_advance_falls_to_the_row_above_when_last_by_path() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "src/parse.rs")));
        app.handle(Action::AcceptFile);
        app.accepted(vec![accepted_ok(
            "alpha",
            2,
            without(pile("alpha"), &["src/parse.rs"]),
        )]);
        assert_eq!(status(&app), "accepted src/parse.rs");
        assert_eq!(
            app.selection,
            Some(row("alpha", "f2")),
            "the row above, not the Root entry"
        );
    }

    /// The sponsor's sentence, in one assertion: "when I accept the last file, the cursor
    /// goes to highlight the repo name" (§6.7, Amendment v1.9 item 1). Never beta's first
    /// row — the cursor does not leave a repo that is still listed.
    #[test]
    fn app_accept_last_file_selects_the_repo_row() {
        let mut app = three_roots();
        app.apply(pile_event("alpha", without(pile("alpha"), &["f2"])));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::AcceptFile);
        app.accepted(vec![accepted_ok("alpha", 2, Pile::default())]);
        assert_eq!(
            app.selection,
            Some(Selection::Root(root("alpha"))),
            "the repo row, not beta's first row"
        );
    }

    /// The same when every other repo is empty too: the cursor still lands on alpha's own
    /// name row rather than on nothing (what the pre-v1.9 rule did, because an emptied
    /// root left the nav and there was nowhere left to go).
    #[test]
    fn app_accept_last_file_lands_on_the_repo_row_with_every_repo_empty() {
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
        assert_eq!(app.selection, Some(row("alpha", "src/parse.rs")));
        app.handle(Action::AcceptFile);
        app.accepted(vec![accepted_ok("alpha", 3, Pile::default())]);
        assert_eq!(app.selection, Some(Selection::Root(root("alpha"))));
        assert_eq!(status(&app), "accepted src/parse.rs");
        assert_eq!(app.nav_entries().len(), 3, "three empty repos, three rows");
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

    /// A second lastcall process held the root's ledger lock (Phase 5 deliverable 2c). The
    /// status line says so in the root's own words, the row is *not* marked seen, and it is
    /// still there to try again — this is deliberately not a `Refused`, which would render
    /// against the row and read like the file changed underneath.
    #[test]
    fn app_accept_refused_by_a_busy_ledger_says_try_again_and_leaves_the_row_pending() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Accept);
        let changed = app.accepted(vec![(root("alpha"), Err(AcceptFailed::LedgerBusy))]);
        assert_eq!(changed, Changed::Yes);
        assert_eq!(status(&app), "ledger busy in alpha — try again");
        assert!(
            app.roots[&root("alpha")].listed(),
            "alpha's pile is untouched, so the row is still pending"
        );
        assert_eq!(
            app.selection,
            Some(row("alpha", "f1")),
            "the cursor did not move on"
        );
        assert_eq!(
            app.accepting, None,
            "the request is over; the key works again"
        );
    }

    /// The classification is on the typed error, not on the message text.
    #[test]
    fn app_accept_failed_classifies_lock_busy_and_nothing_else() {
        use lastcall_engine::paths::RepoPaths;
        let busy = EngineError::Ops(OpsError::Ledger(LedgerError::LockBusy {
            path: RepoPaths::under("/state/repo".into()).lock,
            retries: 40,
        }));
        assert_eq!(AcceptFailed::of(&busy), AcceptFailed::LedgerBusy);
        let other = EngineError::NoSuchRoot("/gone".into());
        assert_eq!(
            AcceptFailed::of(&other),
            AcceptFailed::Other("no such root: /gone".into())
        );
    }

    #[test]
    fn app_multi_root_accepted_with_one_err_applies_others_and_names_failed_root() {
        let mut app = three_roots();
        app.select(Some(row("beta", "u1")));
        app.handle(Action::AcceptAll);
        let changed = app.accepted(vec![
            (root("alpha"), Err(AcceptFailed::Other("boom".into()))),
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
            app.selection,
            Some(Selection::Root(root("beta"))),
            "beta emptied but stayed listed → its own name row"
        );

        // All three fine: one repo count, or the root's name when it is one.
        let mut app = three_roots();
        app.handle(Action::AcceptAll);
        app.accepted(vec![
            accepted_ok("alpha", 2, Pile::default()),
            accepted_ok("beta", 2, Pile::default()),
            accepted_ok("notes", 2, Pile::default()),
        ]);
        assert_eq!(status(&app), "accepted 6 files in 3 repos");
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
                row("alpha", "src/parse.rs"),
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
            Some(Selection::Root(root("alpha"))),
            "the repo is still listed, so the cursor stays in it — its name row"
        );

        app.select(Some(Selection::Group(root("beta"), Annotation::Upstream)));
        let mut p = pile("beta");
        for r in &mut p.rows {
            r.annotation = Some(Annotation::Mixed);
        }
        app.apply(pile_event("beta", p));
        assert_eq!(
            app.selection,
            Some(row("beta", "u2")),
            "a group sits below the rows, so the nearest above it is the last row"
        );

        app.select(Some(row("notes", "n2.md")));
        app.apply(pile_event("notes", Pile::default()));
        assert_eq!(
            app.selection,
            Some(Selection::Root(root("notes"))),
            "the last repo keeps the cursor too"
        );
    }

    /// "Below, else above" in one repo: the last row falls to the row **above** it, and
    /// only when there is no row left at all does the repo's own name row take the cursor
    /// (§6.7, Amendment v1.9 item 4). The pre-v1.9 rule skipped straight to the root entry.
    #[test]
    fn app_last_row_falls_to_the_row_above_then_to_the_repo_row() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "src/parse.rs")));
        let mut p = pile("alpha");
        p.rows.retain(|r| r.path != b"src/parse.rs");
        app.apply(pile_event("alpha", p));
        assert_eq!(app.selection, Some(row("alpha", "f2")), "the row above");
        app.apply(pile_event("alpha", Pile::default()));
        assert_eq!(app.selection, Some(Selection::Root(root("alpha"))));
    }

    /// "Below" wins over "above": a row accepted out of the middle of a repo hands the
    /// cursor to the row **under** it (§6.7, Amendment v1.9 item 4).
    #[test]
    fn app_accept_middle_file_selects_the_row_below() {
        let mut app = three_roots();
        app.apply(pile_event("alpha", rows_n(3, 0, 0)));
        app.select(Some(row("alpha", "p01")));
        app.handle(Action::AcceptFile);
        app.accepted(vec![accepted_ok(
            "alpha",
            2,
            without(rows_n(3, 0, 0), &["p01"]),
        )]);
        assert_eq!(app.selection, Some(row("alpha", "p02")), "below, not above");
    }

    /// The first row has the repo's own name row above it and a file row below it, so the
    /// rule has to pick the file: landing on the name row here would read as the whole
    /// repo emptying when it did not.
    #[test]
    fn app_accept_first_file_selects_the_row_below_not_above() {
        let mut app = three_roots();
        app.apply(pile_event("alpha", rows_n(3, 0, 0)));
        app.select(Some(row("alpha", "p00")));
        app.handle(Action::AcceptFile);
        app.accepted(vec![accepted_ok(
            "alpha",
            2,
            without(rows_n(3, 0, 0), &["p00"]),
        )]);
        assert_eq!(app.selection, Some(row("alpha", "p01")));
        assert_ne!(app.selection, Some(Selection::Root(root("alpha"))));
    }

    /// A pile that adds rows sorting **above** the accepted one moves every nav index and
    /// none of the keys `neighbour_after` compares (design review F7). The cursor still
    /// goes to the row that was below.
    #[test]
    fn app_accept_with_rows_added_above_still_selects_the_row_below() {
        let mut app = three_roots();
        app.apply(pile_event("alpha", rows_n(3, 0, 0)));
        app.select(Some(row("alpha", "p01")));
        app.handle(Action::AcceptFile);
        // The rescan lost `p01` and gained `p000`/`p001`, both sorting before `p01`.
        let mut after = without(rows_n(3, 0, 0), &["p01"]);
        for name in ["p000", "p001"] {
            let mut extra = after.rows[0].clone();
            extra.path = name.as_bytes().to_vec();
            after.rows.push(extra);
        }
        after.rows.sort_by(|a, b| a.path.cmp(&b.path));
        app.accepted(vec![accepted_ok("alpha", 2, after)]);
        assert_eq!(app.selection, Some(row("alpha", "p02")));
    }

    /// `t` (§6.7, Amendment v1.9 item 4): the repos with nothing pending go, except one
    /// whose agent wants attention — and pressing it again brings them back.
    #[test]
    fn app_hide_empty_unlists_a_root_with_no_rows_but_keeps_a_flagged_one() {
        let names = |app: &App| {
            app.listed_roots()
                .map(|v| v.meta.name.clone())
                .collect::<Vec<_>>()
        };
        let mut app = three_roots();
        app.apply(pile_event_seq("beta", 1, Pile::default()));
        app.apply(pile_event_seq("notes", 1, Pile::default()));
        app.handle(Action::Herdr(HerdrUpdate::Connected {
            version: "0.8.2".to_owned(),
            protocol: 21,
        }));
        app.handle(Action::Herdr(HerdrUpdate::Roots(BTreeMap::from([(
            root("notes"),
            hfix::agents(Attention::Blocked, 1, "w1:p1", "claude"),
        )]))));
        assert_eq!(names(&app), ["alpha", "beta", "notes"], "the default");

        assert_eq!(app.handle(Action::HideEmpty).0, Changed::Yes);
        assert!(app.hide_empty);
        assert_eq!(
            names(&app),
            ["alpha", "notes"],
            "beta goes; the blocked agent keeps notes"
        );
        assert_eq!(app.handle(Action::HideEmpty).0, Changed::Yes);
        assert!(!app.hide_empty);
        assert_eq!(names(&app), ["alpha", "beta", "notes"]);
    }

    /// With `t` on there is no repo row to land on, so the third rule runs: the entry now
    /// standing where the accepted row stood.
    #[test]
    fn app_hide_empty_accept_last_file_selects_the_entry_that_takes_its_place() {
        let mut app = three_roots();
        app.hide_empty = true;
        app.apply(pile_event("alpha", without(pile("alpha"), &["f2"])));
        // Nav: [Root(alpha), f1, Root(beta), u1, u2, Group(beta), Root(notes), n2.md].
        app.select(Some(row("alpha", "f1")));
        assert_eq!(app.nav_anchor, Some(1));
        app.handle(Action::AcceptFile);
        app.accepted(vec![accepted_ok("alpha", 2, Pile::default())]);
        assert_eq!(
            app.selection,
            Some(row("beta", "u1")),
            "alpha left the nav, so index 1 is beta's first row now"
        );

        // Nothing left at all: nothing selected.
        let mut app = three_roots();
        app.hide_empty = true;
        app.apply(pile_event("beta", Pile::default()));
        app.apply(pile_event("notes", Pile::default()));
        app.select(Some(row("alpha", "f2")));
        app.apply(pile_event_seq("alpha", 1, Pile::default()));
        assert_eq!(app.selection, None);
    }

    /// One rule, two entry points: a pile that arrives on its own (the watcher saw an
    /// external commit take several rows) moves the cursor exactly where an accept of the
    /// same rows would have — `advance_after` no longer owns a second, differently
    /// ordered fallback (kickoff deliverable 2.3).
    #[test]
    fn app_reconcile_uses_the_same_neighbour_rule_as_advance() {
        let external = {
            let mut app = three_roots();
            app.apply(pile_event("alpha", rows_n(4, 0, 0)));
            app.select(Some(row("alpha", "p01")));
            // Someone committed p00, p01 and p02 outside lastcall.
            app.apply(pile_event_seq(
                "alpha",
                1,
                without(rows_n(4, 0, 0), &["p00", "p01", "p02"]),
            ));
            app.selection.clone()
        };
        let accepted = {
            let mut app = three_roots();
            app.apply(pile_event("alpha", rows_n(4, 0, 0)));
            app.select(Some(row("alpha", "p01")));
            app.handle(Action::AcceptFile);
            app.accepted(vec![accepted_ok(
                "alpha",
                1,
                without(rows_n(4, 0, 0), &["p00", "p01", "p02"]),
            )]);
            app.selection.clone()
        };
        assert_eq!(external, Some(row("alpha", "p03")));
        assert_eq!(external, accepted, "one rule, both paths");

        // And when the whole repo empties, both land on its name row.
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        app.apply(pile_event_seq("alpha", 1, Pile::default()));
        assert_eq!(app.selection, Some(Selection::Root(root("alpha"))));
    }

    /// `w` and `t` are independent filters (§6.7, Amendment v1.9): the scope decides which
    /// repos are in play, `t` decides whether the empty ones among those show. Neither
    /// undoes the other, in any order.
    #[test]
    fn app_scope_and_hide_empty_are_independent() {
        let names = |app: &App| {
            app.listed_roots()
                .map(|v| v.meta.name.clone())
                .collect::<Vec<_>>()
        };
        let mut app = three_roots();
        app.apply(pile_event_seq("beta", 1, Pile::default()));
        app.herdr.scoped = true;
        app.handle(Action::Herdr(HerdrUpdate::Scope(Some(Scope {
            label: "w1".to_owned(),
            roots: [root("alpha"), root("beta")].into_iter().collect(),
        }))));
        assert_eq!(names(&app), ["alpha", "beta"], "notes is out of scope");

        // `t`: beta is empty and in scope → it goes. notes is still out of scope.
        app.handle(Action::HideEmpty);
        assert_eq!(names(&app), ["alpha"]);

        // `w` with `t` still on: the scope lifts, and `t` keeps hiding the empty ones —
        // notes has rows, so it comes back.
        app.handle(Action::ScopeToggle);
        assert_eq!(names(&app), ["alpha", "notes"]);
        assert_eq!(app.scope_notice(), None, "no scope in force");

        // `w` back on, then `t` off: the scope is unchanged by the round trip.
        app.handle(Action::ScopeToggle);
        assert_eq!(names(&app), ["alpha"]);
        app.handle(Action::HideEmpty);
        assert_eq!(names(&app), ["alpha", "beta"]);
        assert_eq!(
            app.scope_notice().as_deref(),
            Some("scope: w1 · 1 repo hidden (w shows all)"),
            "the notice counts what the scope hides, never what `t` does"
        );
    }

    /// Verifier (a) F1: the notice promises what `w` will reveal. Since v1.9 an **empty**
    /// out-of-scope repo is one of them while `hide_empty` is off, so the count has to be
    /// `is_listed`'s rule minus the scope test — not the pre-v1.9 "has rows or has a flag".
    #[test]
    fn app_scope_notice_counts_an_empty_out_of_scope_repo() {
        let names = |app: &App| {
            app.listed_roots()
                .map(|v| v.meta.name.clone())
                .collect::<Vec<_>>()
        };
        let mut app = three_roots();
        // notes is the out-of-scope one, and it is empty.
        app.apply(pile_event_seq("notes", 1, Pile::default()));
        app.herdr.scoped = true;
        app.handle(Action::Herdr(HerdrUpdate::Scope(Some(Scope {
            label: "w1".to_owned(),
            roots: [root("alpha"), root("beta")].into_iter().collect(),
        }))));
        assert_eq!(names(&app), ["alpha", "beta"]);
        assert_eq!(
            app.scope_notice().as_deref(),
            Some("scope: w1 · 1 repo hidden (w shows all)"),
            "the empty out-of-scope repo is what `w` reveals"
        );
        // …and `w` does reveal exactly it.
        app.handle(Action::ScopeToggle);
        assert_eq!(names(&app), ["alpha", "beta", "notes"]);

        // With `t` on it would not be revealed, so the notice must not promise it.
        app.handle(Action::ScopeToggle);
        app.handle(Action::HideEmpty);
        assert_eq!(
            app.scope_notice().as_deref(),
            Some("scope: w1 · 0 repos hidden (w shows all)")
        );
        app.handle(Action::ScopeToggle);
        assert_eq!(names(&app), ["alpha", "beta"], "`t` still hides notes");
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

    /// Ruling R2, the sponsor verbatim: "always display the git repo … even if there's no
    /// changes staged within it". An emptied repo keeps exactly one nav entry — its own
    /// name row — and that row is selectable by key and by click.
    #[test]
    fn app_empty_root_is_listed_and_selectable() {
        let mut app = three_roots();
        assert!(app.nav_entries().iter().any(|e| e.root() == root("beta")));
        app.apply(pile_event("beta", Pile::default()));
        assert!(app.roots.contains_key(&root("beta")), "the view stays");
        assert!(!app.roots[&root("beta")].listed(), "no rows");
        let entries = app.nav_entries();
        let beta: Vec<&Selection> = entries
            .iter()
            .filter(|e| e.root() == root("beta"))
            .collect();
        assert_eq!(
            beta,
            vec![&Selection::Root(root("beta"))],
            "one entry: the name row"
        );
        assert_eq!(
            app.select(Some(Selection::Root(root("beta")))),
            Changed::Yes,
            "selectable by key"
        );
        assert_eq!(
            app.hit(Target::NavRoot(root("beta"))).0,
            Changed::No,
            "already there; the click resolves to the same entry"
        );
        assert_eq!(app.selection, Some(Selection::Root(root("beta"))));
    }

    #[test]
    fn app_in_progress_tag_never_lists_or_unlists_a_root() {
        let mut app = three_roots();
        // Under `t` an empty pile hides the repo; the tag alone must not bring it back
        // (only an attention flag does — Amendment v1.9 item 4).
        app.hide_empty = true;
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
        assert_eq!(
            app.selection,
            Some(row("notes", "n2.md")),
            "beta left the nav entirely, so the cursor takes the entry now standing at its \
             former nav index (§6.7, Amendment v1.9 item 4, the third rule)"
        );

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

    /// The jumps (2026-09-14) with the nav focused: `home` and `end` reach the ends of
    /// `nav_entries`, `}` walks the repository rows forward and `{` back, neither wraps,
    /// and `{` from inside the first repository is that repository's own row.
    #[test]
    fn app_nav_jumps_reach_the_ends_and_walk_the_repository_rows() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        let entries = app.nav_entries();
        let roots = [root("alpha"), root("beta"), root("notes")];
        assert_eq!(entries.first(), Some(&Selection::Root(root("alpha"))));

        assert_eq!(app.handle(Action::NavBottom).0, Changed::Yes);
        assert_eq!(app.selection.as_ref(), entries.last());
        assert_eq!(
            app.handle(Action::NavBottom).0,
            Changed::No,
            "the last entry is already the last entry"
        );
        assert_eq!(app.handle(Action::NavTop).0, Changed::Yes);
        assert_eq!(app.selection.as_ref(), entries.first());
        assert_eq!(app.handle(Action::NavTop).0, Changed::No);

        for next in &roots[1..] {
            assert_eq!(app.handle(Action::NavNextRoot).0, Changed::Yes);
            assert_eq!(app.selection, Some(Selection::Root(next.clone())));
        }
        assert_eq!(
            app.handle(Action::NavNextRoot).0,
            Changed::No,
            "the last repository is where `}}` stops"
        );
        assert_eq!(app.selection, Some(Selection::Root(root("notes"))));
        for prev in [root("beta"), root("alpha")] {
            assert_eq!(app.handle(Action::NavPrevRoot).0, Changed::Yes);
            assert_eq!(app.selection, Some(Selection::Root(prev)));
        }
        assert_eq!(
            app.handle(Action::NavPrevRoot).0,
            Changed::No,
            "and the first is where `{{` stops"
        );

        // From inside a repository: `{` is the previous repository's row, and inside the
        // first one it is that repository's own.
        app.select(Some(row("beta", "u2")));
        assert_eq!(app.handle(Action::NavPrevRoot).0, Changed::Yes);
        assert_eq!(app.selection, Some(Selection::Root(root("alpha"))));
        app.select(Some(row("alpha", "f2")));
        assert_eq!(app.handle(Action::NavPrevRoot).0, Changed::Yes);
        assert_eq!(
            app.selection,
            Some(Selection::Root(root("alpha"))),
            "inside the first repository `{{` is its own head"
        );
        app.select(Some(row("alpha", "f2")));
        assert_eq!(app.handle(Action::NavNextRoot).0, Changed::Yes);
        assert_eq!(
            app.selection,
            Some(Selection::Root(root("beta"))),
            "and `}}` is the next repository's, not this one's"
        );
    }

    /// The same four with the **diff** focused: the ends move the pane and leave the
    /// selection alone, and the repository jumps bring the keys back to the nav with them
    /// (a repository row has no diff to read).
    #[test]
    fn app_nav_jumps_from_the_diff_scroll_it_and_come_back_to_the_nav() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::FocusToggle);
        assert_eq!(app.effective_focus(), Focus::Diff);
        let lines = diff_lines(app.view_hunks());
        assert!(lines > 1, "f1 has a diff to scroll");

        assert_eq!(app.handle(Action::NavBottom).0, Changed::Yes);
        assert_eq!(app.diff.scroll, lines - 1);
        assert_eq!(app.handle(Action::NavBottom).0, Changed::No);
        // The same place a long `↓` run ends.
        app.handle(Action::NavTop);
        for _ in 0..lines + 5 {
            app.handle(Action::NavDown);
        }
        assert_eq!(app.diff.scroll, lines - 1, "`end` is where `↓` gives up");
        assert_eq!(app.handle(Action::NavTop).0, Changed::Yes);
        assert_eq!(app.diff.scroll, 0);
        assert_eq!(app.handle(Action::NavTop).0, Changed::No);
        assert_eq!(
            app.selection,
            Some(row("alpha", "f1")),
            "neither end touched the selection"
        );
        assert_eq!(app.focus, Focus::Diff, "nor the focus");

        assert_eq!(app.handle(Action::NavNextRoot).0, Changed::Yes);
        assert_eq!(app.selection, Some(Selection::Root(root("beta"))));
        assert_eq!(app.focus, Focus::Nav, "`}}` hands the keys back to the nav");

        app.select(Some(Selection::Root(root("alpha"))));
        app.handle(Action::FocusToggle);
        assert_eq!(app.focus, Focus::Diff);
        assert_eq!(
            app.handle(Action::NavPrevRoot).0,
            Changed::Yes,
            "the focus alone is a change"
        );
        assert_eq!(app.selection, Some(Selection::Root(root("alpha"))));
        assert_eq!(app.focus, Focus::Nav);
    }

    /// A root the nav is not showing is no stop on the way, exactly as `↑`/`↓` skip it,
    /// and an empty nav answers `Changed::No` to all four.
    #[test]
    fn app_nav_jumps_skip_a_hidden_root_and_an_empty_nav_is_nothing() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.apply(pile_event(
            "beta",
            snoozed_pile(pile("beta"), "2026-12-01T00:00:00Z"),
        ));
        let beta = root("beta");
        assert!(
            !app.nav_entries().iter().any(|e| e.root() == beta),
            "beta is snoozed off the nav"
        );
        app.select(Some(Selection::Root(root("alpha"))));
        assert_eq!(app.handle(Action::NavNextRoot).0, Changed::Yes);
        assert_eq!(
            app.selection,
            Some(Selection::Root(root("notes"))),
            "the snoozed repository is not a stop"
        );
        assert_eq!(app.handle(Action::NavPrevRoot).0, Changed::Yes);
        assert_eq!(app.selection, Some(Selection::Root(root("alpha"))));
        app.handle(Action::NavBottom);
        let notes = root("notes");
        assert_eq!(
            app.selection.as_ref().map(Selection::root),
            Some(notes.as_path()),
            "`end` skips it too"
        );

        let mut empty = App::new();
        assert!(empty.nav_entries().is_empty());
        for action in [
            Action::NavTop,
            Action::NavBottom,
            Action::NavPrevRoot,
            Action::NavNextRoot,
        ] {
            assert_eq!(
                empty.handle(action.clone()).0,
                Changed::No,
                "{action:?} on an empty nav"
            );
            assert_eq!(empty.selection, None);
        }
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
        // The mode line, then the blank separator, then the second hunk's header.
        assert_eq!(
            hunk_offsets(&[mode.clone(), row.hunks[0].clone()]),
            vec![0, 2]
        );
        assert_eq!(hunk_block(&[mode.clone(), row.hunks[0].clone()], 0), 2);
        assert_eq!(hunk_block(&[mode.clone()], 0), 1, "the last hunk has none");
    }

    // --- herdr (deliverables 5 and 8) ----------------------------------------------------

    use super::super::herdr::testfix as hfix;
    use super::super::herdr::{Attention, Dot, Ready, RootAgents, Scope, ToastShown};

    /// `three_roots` with alpha emptied, herdr connected, and `derived` folded in.
    fn with_herdr(derived: BTreeMap<PathBuf, RootAgents>) -> App {
        let mut app = three_roots();
        assert_eq!(
            app.apply(pile_event_seq(
                "alpha",
                1,
                without(pile("alpha"), &["f1", "f2", "src/parse.rs"])
            ))
            .0,
            Changed::Yes,
            "alpha now has nothing pending"
        );
        app.handle(Action::Herdr(HerdrUpdate::Connected {
            version: "0.8.2".to_owned(),
            protocol: 21,
        }));
        app.handle(Action::Herdr(HerdrUpdate::Roots(derived)));
        app
    }

    fn one(name: &str, status: Attention) -> BTreeMap<PathBuf, RootAgents> {
        BTreeMap::from([(root(name), hfix::agents(status, 1, "w1:p1", "claude"))])
    }

    /// Ruling 4 + ruling 9: a repo with nothing pending is listed because its agent is
    /// done, and the nav row says so instead of showing a pile.
    #[test]
    fn app_herdr_done_lists_a_root_with_nothing_pending() {
        let app = with_herdr(one("alpha", Attention::Done));
        assert!(
            app.listed_roots().any(|v| v.meta.path == root("alpha")),
            "a done agent lists its repo"
        );
        assert_eq!(
            app.herdr.dot(&root("alpha")),
            Some(Dot::Ready { acked: false })
        );
        assert!(app.nav_entries().contains(&Selection::Root(root("alpha"))));
    }

    /// Ruling 4: `working` and `idle` only annotate. A repo with nothing pending and a
    /// working agent stays off the list; `blocked` puts it back on.
    #[test]
    fn app_herdr_working_annotates_but_blocked_lists() {
        // Since Amendment v1.9 every repo is listed by default, so this rule is what `t`
        // keeps: `blocked` (and `done`) survive `hide_empty`, `working`/`idle` do not.
        let with_herdr = |m| {
            let mut app = with_herdr(m);
            app.hide_empty = true;
            app
        };
        let app = with_herdr(one("alpha", Attention::Working));
        assert!(
            !app.listed_roots().any(|v| v.meta.path == root("alpha")),
            "a working agent is not a reason to list an empty repo"
        );
        assert_eq!(app.herdr.dot(&root("alpha")), Some(Dot::Working));

        let app = with_herdr(one("alpha", Attention::Blocked));
        assert!(app.listed_roots().any(|v| v.meta.path == root("alpha")));
        assert_eq!(app.herdr.dot(&root("alpha")), Some(Dot::Blocked));

        let app = with_herdr(one("alpha", Attention::Idle));
        assert!(!app.listed_roots().any(|v| v.meta.path == root("alpha")));
        assert_eq!(app.herdr.dot(&root("alpha")), None);
    }

    /// Ruling 10: `d` is local and idempotent — the dot dims, herdr is never told, and the
    /// repo stays listed. The name also leaves any pending toast window.
    #[test]
    fn app_herdr_ack_dims_the_flag_and_withdraws_the_toast() {
        let mut app = with_herdr(one("alpha", Attention::Done));
        app.select(Some(Selection::Root(root("alpha"))));
        let (changed, effect) = app.handle(Action::Ack);
        assert_eq!(changed, Changed::Yes);
        assert_eq!(
            effect,
            Some(Effect::Toast(ToastRequest {
                ready: Vec::new(),
                dropped: vec![root("alpha")],
            }))
        );
        assert_eq!(
            app.herdr.dot(&root("alpha")),
            Some(Dot::Ready { acked: true })
        );
        assert!(
            app.listed_roots().any(|v| v.meta.path == root("alpha")),
            "an acked flag still lists the repo; it only stops shouting"
        );
        assert_eq!(
            app.handle(Action::Ack),
            (Changed::No, None),
            "acking twice is not news"
        );
        // Nothing about the ack reaches herdr: the only effect it can produce is the toast
        // withdrawal, and the flag herdr sees is still `done`.
        assert_eq!(
            app.herdr.flag(&root("alpha")).unwrap().status,
            Attention::Done
        );
    }

    /// Deliverable 9: the nav offset belongs to the frame, not to the reducers. `sync_roots`
    /// and `apply` must leave it alone — a list that shrank under it is `render_nav`'s clamp
    /// to fix, and a reducer that "helpfully" reset it would undo the reader's scroll on
    /// every pile.
    #[test]
    fn app_reducers_never_move_the_nav_offset() {
        let mut app = three_roots();
        app.nav_top = 7;
        app.sync_roots(vec![meta("alpha"), meta("beta")]);
        app.apply(pile_event_seq("alpha", 9, rows_n(3, 0, 0)));
        app.select(Some(row("alpha", "p01")));
        app.handle(Action::Resize(80, 24));
        app.handle(Action::NavPageDown);
        assert_eq!(app.nav_top, 7, "only a drawn frame moves it");
    }

    /// Phase 6 deliverable 6: herdr re-derives on every snapshot it receives, and most say
    /// what the last one said. The reducer answers `Changed::No` for those, so the loop
    /// does not repaint on a poll that moved nothing.
    #[test]
    fn app_herdr_roots_draw_nothing_when_the_derivation_is_identical() {
        let mut app = with_herdr(BTreeMap::new());
        let done = || Action::Herdr(HerdrUpdate::Roots(one("alpha", Attention::Done)));
        assert_eq!(app.handle(done()).0, Changed::Yes, "the first flag lights");
        assert_eq!(
            app.handle(done()).0,
            Changed::No,
            "the same derivation again is the same frame"
        );
        assert_eq!(
            app.handle(Action::Herdr(HerdrUpdate::Roots(one(
                "alpha",
                Attention::Working
            ))))
            .0,
            Changed::Yes,
            "a status change moves the dot"
        );
    }

    /// Deliverable 6: the reducer asks for a toast on the episodes that just opened, and
    /// only while `[herdr] toast` is on.
    #[test]
    fn app_herdr_toast_effect_names_only_the_opened_episodes() {
        let mut app = with_herdr(BTreeMap::new());
        app.herdr.toast = true;
        let (_, effect) = app.handle(Action::Herdr(HerdrUpdate::Roots(one(
            "alpha",
            Attention::Done,
        ))));
        assert_eq!(
            effect,
            Some(Effect::Toast(ToastRequest {
                ready: vec![(root("alpha"), "alpha".to_owned())],
                dropped: Vec::new(),
            }))
        );
        // The same derivation again is the same episode: nothing to send.
        let (_, effect) = app.handle(Action::Herdr(HerdrUpdate::Roots(one(
            "alpha",
            Attention::Done,
        ))));
        assert_eq!(effect, None);
        // The agent went back to work: the episode closed, so the pending name is withdrawn.
        let (_, effect) = app.handle(Action::Herdr(HerdrUpdate::Roots(one(
            "alpha",
            Attention::Working,
        ))));
        assert_eq!(
            effect,
            Some(Effect::Toast(ToastRequest {
                ready: Vec::new(),
                dropped: vec![root("alpha")],
            }))
        );

        app.herdr.toast = false;
        let (changed, effect) = app.handle(Action::Herdr(HerdrUpdate::Roots(one(
            "alpha",
            Attention::Done,
        ))));
        assert_eq!(changed, Changed::Yes, "the flag still lights");
        assert_eq!(effect, None, "[herdr] toast = false sends nothing");
    }

    /// `g` hands the loop the winning agent's **pane id** — herdr's own public id, never a
    /// display name — and the answer names the agent in the status line.
    #[test]
    fn app_herdr_jump_carries_the_pane_id_and_the_answer_is_a_status() {
        let mut app = with_herdr(one("alpha", Attention::Done));
        app.select(Some(Selection::Root(root("alpha"))));
        assert_eq!(
            app.handle(Action::Jump),
            (Changed::No, Some(Effect::Focus("w1:p1".to_owned())))
        );
        app.handle(Action::Herdr(HerdrUpdate::Focused(Ok("claude".to_owned()))));
        assert_eq!(status(&app), "focused claude in herdr");
        app.handle(Action::Herdr(HerdrUpdate::Focused(Err(
            "pane_not_found".to_owned()
        ))));
        assert_eq!(status(&app), "jump failed: pane_not_found");

        // A root herdr says nothing about has nothing to jump to.
        app.select(Some(Selection::Root(root("beta"))));
        assert_eq!(app.handle(Action::Jump), (Changed::No, None));
        assert_eq!(app.handle(Action::Ack), (Changed::No, None));
    }

    /// Deliverable 8: the scope hides the roots outside it, the notice counts exactly what
    /// it hid, and `w` shows all. Without a derived scope `w` does nothing at all.
    /// The launch hold (Gate 8 sponsor run ruling): nothing is listed until every root has
    /// reported — by a `Scanned` tick, its pile, or a scan-failed notice; a global notice
    /// ends the hold outright; no roots means no hold; the clock redraws while it is on.
    /// Design pass D5 (ruling R7): an outstanding herdr scope verdict holds the frame open
    /// past the last report, and `scope_settled` ends it.
    #[test]
    fn app_loading_holds_the_listing_until_every_root_reports() {
        let launched = || {
            let mut app = App::new();
            app.sync_roots(vec![meta("alpha"), meta("beta")]);
            app.start_loading();
            assert!(app.loading.is_some());
            app
        };
        let listed = |app: &App| {
            app.listed_roots()
                .map(|v| v.meta.name.clone())
                .collect::<Vec<_>>()
        };

        let mut app = launched();
        assert_eq!(
            app.handle(Action::Tick).0,
            Changed::Yes,
            "the counter ticks"
        );
        app.apply(pile_event("alpha", pile("alpha")));
        assert!(listed(&app).is_empty(), "held: beta has not reported");
        assert_eq!(
            app.loading.as_ref().unwrap().checked.get(&root("alpha")),
            Some(&pile("alpha").rows.len())
        );
        assert_eq!(
            app.apply(EngineEvent::Scanned {
                root: root("beta"),
                rows: 3
            })
            .0,
            Changed::Yes
        );
        assert!(app.loading.is_none(), "every root reported");
        assert_eq!(
            listed(&app),
            vec!["alpha".to_owned(), "beta".to_owned()],
            "Amendment v1.9: beta reported nothing pending and is listed all the same"
        );
        assert_eq!(
            app.apply(EngineEvent::Scanned {
                root: root("beta"),
                rows: 3
            }),
            (Changed::No, None),
            "a tick after the hold is nothing"
        );

        // A failed scan is that root's report; a global notice ends the hold outright.
        let mut app = launched();
        app.apply(EngineEvent::Notice {
            root: Some(root("alpha")),
            text: "scan failed: boom".into(),
        });
        assert_eq!(
            app.loading.as_ref().unwrap().checked.get(&root("alpha")),
            Some(&0)
        );
        app.apply(EngineEvent::Notice {
            root: None,
            text: "watching /W (2 roots)".into(),
        });
        assert!(app.loading.is_none());

        // No roots: nothing to wait for.
        let mut app = App::new();
        app.start_loading();
        assert!(app.loading.is_none());

        // Design pass D5 / ruling R7: with the herdr scope verdict still outstanding the
        // hold's value survives the last report — flagged `scanned`, which is what makes
        // the pane keep the hold's frame instead of showing a second waiting screen — and
        // `scope_settled` is what ends it.
        let mut app = launched();
        app.herdr.scoped = true;
        app.herdr.scope_pending = true;
        app.apply(pile_event("alpha", pile("alpha")));
        app.apply(EngineEvent::Scanned {
            root: root("beta"),
            rows: 0,
        });
        assert!(
            app.loading.as_ref().is_some_and(|l| l.scanned),
            "every root reported, and the hold is held open for the verdict"
        );
        assert!(listed(&app).is_empty());
        assert_eq!(app.scope_settled(), Changed::Yes);
        assert!(app.loading.is_none(), "the verdict ends the hold");
        assert_eq!(listed(&app), vec!["alpha".to_owned(), "beta".to_owned()]);
    }

    /// The Gate 8 sponsor run's launch flash: the first pile landed before the first scope
    /// verdict, was listed, and was hidden a moment later. While `scope_pending` nothing is
    /// listed; the first verdict lists — even a `None` equal to the starting value — and so
    /// does a link that will never deliver one (standalone, or dropped before its snapshot).
    #[test]
    fn app_scope_pending_holds_the_listing_until_the_first_verdict() {
        let launched = || {
            let mut app = App::new();
            app.herdr.scoped = true;
            app.herdr.scope_pending = true;
            app.sync_roots(vec![meta("alpha"), meta("beta")]);
            app.apply(pile_event("alpha", pile("alpha")));
            assert_eq!(app.listed_roots().count(), 0, "held back until the verdict");
            assert_eq!(app.scope_notice(), None, "no scope is active yet");
            app
        };
        let listed = |app: &App| {
            app.listed_roots()
                .map(|v| v.meta.name.clone())
                .collect::<Vec<_>>()
        };

        let mut app = launched();
        assert_eq!(
            app.handle(Action::Herdr(HerdrUpdate::Scope(None))).0,
            Changed::Yes,
            "the first verdict redraws even when it is the `None` the view started with"
        );
        assert!(!app.herdr.scope_pending);
        assert_eq!(
            listed(&app),
            vec!["alpha".to_owned(), "beta".to_owned()],
            "Amendment v1.9: beta has no pile yet and is listed all the same"
        );
        assert_eq!(
            app.handle(Action::Herdr(HerdrUpdate::Scope(None))),
            (Changed::No, None),
            "the same verdict again is not a redraw"
        );
        assert_eq!(app.scope_settled(), Changed::No, "nothing was pending");

        // A scope that hides the pile: held back, then hidden — never listed in between.
        let mut app = launched();
        let beta = Scope {
            label: "beta".to_owned(),
            roots: [root("beta")].into_iter().collect(),
        };
        assert_eq!(
            app.handle(Action::Herdr(HerdrUpdate::Scope(Some(beta)))).0,
            Changed::Yes
        );
        assert_eq!(
            listed(&app),
            vec!["beta".to_owned()],
            "the scope keeps beta — listed though it has nothing pending — and drops alpha"
        );
        assert_eq!(app.scoped_out(), 1, "alpha has rows and the scope hides it");

        // No verdict is ever coming.
        for update in [
            HerdrUpdate::Standalone {
                reason: "off".to_owned(),
            },
            HerdrUpdate::Reconnecting,
        ] {
            let mut app = launched();
            app.handle(Action::Herdr(update.clone()));
            assert!(!app.herdr.scope_pending, "{update:?}");
            assert_eq!(
                listed(&app),
                vec!["alpha".to_owned(), "beta".to_owned()],
                "{update:?}"
            );
        }
    }

    #[test]
    fn app_herdr_scope_hides_roots_and_w_shows_all() {
        let mut app = three_roots();
        app.herdr.scoped = true;
        assert_eq!(app.scope_notice(), None, "no scope, no notice");
        assert_eq!(app.handle(Action::ScopeToggle), (Changed::No, None));

        let scope = Scope {
            label: "alpha".to_owned(),
            roots: [root("alpha")].into_iter().collect(),
        };
        assert_eq!(
            app.handle(Action::Herdr(HerdrUpdate::Scope(Some(scope.clone()))))
                .0,
            Changed::Yes
        );
        assert_eq!(
            app.listed_roots()
                .map(|v| v.meta.name.clone())
                .collect::<Vec<_>>(),
            vec!["alpha".to_owned()]
        );
        assert_eq!(app.scoped_out(), 2);
        assert_eq!(
            app.scope_notice().as_deref(),
            Some("scope: alpha · 2 repos hidden (w shows all)"),
            "the notice is mandatory whenever the scope hides anything"
        );

        assert_eq!(app.handle(Action::ScopeToggle).0, Changed::Yes);
        assert_eq!(app.listed_roots().count(), 3, "w shows all");
        assert_eq!(app.scope_notice(), None);
        assert_eq!(
            app.handle(Action::Herdr(HerdrUpdate::Scope(Some(scope)))),
            (Changed::No, None),
            "re-deriving the same scope while it is off is not a redraw"
        );
    }

    /// Deliverable 8: "accept-all under scope covers listed roots only". `^A` folds every
    /// **listed** root, and the scope is what decides listing — a hidden root's rows are
    /// neither accepted nor named in the confirm modal.
    #[test]
    fn app_herdr_accept_all_under_scope_covers_listed_roots_only() {
        let mut app = three_roots();
        app.herdr.scoped = true;
        app.handle(Action::Herdr(HerdrUpdate::Scope(Some(Scope {
            label: "alpha".to_owned(),
            roots: [root("alpha")].into_iter().collect(),
        }))));
        assert_eq!(app.listed_roots().count(), 1, "beta and notes are hidden");

        // The requests the reducer would hand the engine name alpha and nothing else.
        assert_eq!(
            app.accept_requests(&AcceptScope::All)
                .into_iter()
                .map(|(r, _)| r)
                .collect::<Vec<_>>(),
            vec![root("alpha")],
            "a hidden root is not accepted behind the user's back"
        );
        // And so do the confirm modal's numbers: alpha's three rows, alpha's name.
        let counts = app.counts_of(&AcceptScope::All);
        assert_eq!(counts.roots, vec!["alpha".to_owned()]);
        assert_eq!(
            counts.files, 3,
            "beta's two and notes' one are out of scope"
        );

        // Above the threshold the modal shows that same tally, and `y` accepts that set.
        app.apply(pile_event_seq("alpha", 1, rows_n(11, 0, 0)));
        assert_eq!(app.handle(Action::AcceptAll).1, None, "eleven files ask");
        let counts = app.confirm_counts().expect("the modal is open");
        assert_eq!(counts.roots, vec!["alpha".to_owned()]);
        assert_eq!(counts.files, 11);
        let (_, effect) = app.handle(Action::Confirm);
        let Some(Effect::Accept(reqs)) = effect else {
            panic!("y starts the accept: {effect:?}")
        };
        assert_eq!(
            reqs.into_iter().map(|(r, _)| r).collect::<Vec<_>>(),
            vec![root("alpha")]
        );

        // `w` shows all three again, and then accept-all covers all three.
        app.accepting = None;
        app.handle(Action::ScopeToggle);
        assert_eq!(app.counts_of(&AcceptScope::All).roots.len(), 3);
        assert_eq!(app.accept_requests(&AcceptScope::All).len(), 3);
    }

    /// The confirm modal and the help overlay are the user's own state: herdr news folds
    /// in underneath them and never closes either.
    #[test]
    fn app_herdr_update_never_closes_the_modal_or_the_help() {
        let mut app = three_roots();
        app.apply(pile_event_seq("alpha", 1, rows_n(11, 0, 0)));
        app.handle(Action::AcceptAll);
        assert!(app.confirm.is_some(), "eleven files ask");
        assert_eq!(
            app.handle(Action::Herdr(HerdrUpdate::Roots(one(
                "beta",
                Attention::Done
            ))))
            .0,
            Changed::Yes
        );
        assert!(app.confirm.is_some(), "the modal survives herdr news");
        assert!(app.herdr.flag(&root("beta")).unwrap().ready.is_some());
        app.handle(Action::Cancel);

        app.handle(Action::Help);
        assert!(app.help);
        app.handle(Action::Herdr(HerdrUpdate::Reconnecting));
        assert!(app.help, "the help overlay survives herdr news");
        assert_eq!(app.herdr.link, Link::Reconnecting);
    }

    /// herdr can answer before the first scan does. A derivation naming a root the app has
    /// not adopted yet is kept, and lights the moment `sync_roots` learns the root.
    #[test]
    fn app_herdr_update_before_sync_roots_is_kept() {
        let mut app = App::new();
        app.handle(Action::Herdr(HerdrUpdate::Connected {
            version: "0.8.2".to_owned(),
            protocol: 21,
        }));
        app.handle(Action::Herdr(HerdrUpdate::Roots(one(
            "alpha",
            Attention::Done,
        ))));
        assert!(
            app.herdr.flag(&root("alpha")).is_some(),
            "the flag is not dropped on the floor for want of a root"
        );
        assert_eq!(app.listed_roots().count(), 0, "no roots to list yet");

        assert_eq!(app.sync_roots(vec![meta("alpha")]), Changed::Yes);
        assert!(
            app.listed_roots().any(|v| v.meta.path == root("alpha")),
            "the moment the root exists, the flag lists it"
        );
        assert_eq!(
            app.herdr.dot(&root("alpha")),
            Some(Dot::Ready { acked: false })
        );
    }

    /// A link that drops takes every dot with it (§6.6 degradation) but not the ack
    /// episodes: reconnecting re-derives, and an ack the user already made still stands.
    #[test]
    fn app_herdr_reconnect_neutralises_the_dots_and_keeps_the_acks() {
        let mut app = with_herdr(one("alpha", Attention::Done));
        app.select(Some(Selection::Root(root("alpha"))));
        app.handle(Action::Ack);
        app.handle(Action::Herdr(HerdrUpdate::Reconnecting));
        assert_eq!(app.herdr.dot(&root("alpha")), None);
        assert_eq!(
            app.herdr.flag(&root("alpha")).unwrap().ready,
            Some(Ready { acked: true })
        );
        app.handle(Action::Herdr(HerdrUpdate::Connected {
            version: "0.8.2".to_owned(),
            protocol: 21,
        }));
        assert_eq!(
            app.herdr.dot(&root("alpha")),
            Some(Dot::Ready { acked: true })
        );
    }

    /// A shown toast says so; a refusal is the task's debug log, not a banner.
    #[test]
    fn app_herdr_toast_verdict_only_speaks_when_it_was_shown() {
        let mut app = three_roots();
        app.handle(Action::Herdr(HerdrUpdate::Toast(Ok(ToastShown {
            shown: false,
            reason: "busy".to_owned(),
        }))));
        assert_eq!(status(&app), "");
        assert_eq!(
            app.handle(Action::Herdr(HerdrUpdate::Toast(Err("eof".to_owned())))),
            (Changed::No, None)
        );
        app.handle(Action::Herdr(HerdrUpdate::Toast(Ok(ToastShown {
            shown: true,
            reason: String::new(),
        }))));
        assert_eq!(status(&app), "toast shown");
    }

    /// Clicking the header badge says what the link is doing, and says why when `mode = on`
    /// made a failure visible.
    #[test]
    fn app_herdr_header_badge_click_names_the_link() {
        let mut app = three_roots();
        app.handle(Action::Herdr(HerdrUpdate::Connected {
            version: "0.8.2".to_owned(),
            protocol: 21,
        }));
        assert_eq!(app.hit(Target::HeaderHerdr).0, Changed::Yes);
        assert_eq!(status(&app), "herdr 0.8.2");
        app.handle(Action::Herdr(HerdrUpdate::Standalone {
            reason: "no socket at /run/herdr.sock".to_owned(),
        }));
        app.hit(Target::HeaderHerdr);
        assert_eq!(status(&app), "standalone: no socket at /run/herdr.sock");
        // A silent standalone (the default `auto`) has nothing to say.
        app.handle(Action::Herdr(HerdrUpdate::Standalone {
            reason: String::new(),
        }));
        app.status = None;
        assert_eq!(app.hit(Target::HeaderHerdr), (Changed::No, None));
    }
    #[test]
    fn app_range_label_matches_git() {
        assert_eq!(range_label(0, 3), "1,3");
        assert_eq!(range_label(0, 0), "0,0");
        assert_eq!(range_label(4, 0), "4,0");
        assert_eq!(range_label(9, 1), "10", "git omits `,1`");
    }

    // ---- restore and flag (Phase 7) -----------------------------------------------------

    /// One `Flagged` answer: a clean outcome, the export the engine computed, and `pile` as
    /// the rescan that followed.
    fn flagged_ok(export: &str, seq: u64, pile: Pile) -> FlagResult {
        Ok(Flagged {
            outcome: lastcall_engine::ops::Outcome::default(),
            export: export.to_owned(),
            seq,
            pile,
        })
    }

    /// The kind the loop builds for an `Effect::Flag` with this label.
    fn flag_of(label: &str) -> FlagKind {
        FlagKind::Flag {
            label: label.to_owned(),
        }
    }

    /// One clean `Restored` answer.
    fn restored_ok(seq: u64, pile: Pile) -> RestoreResult {
        Ok(Restored {
            outcome: lastcall_engine::ops::Outcome::default(),
            seq,
            pile,
        })
    }

    fn agent(pane: &str, label: &str, workspace: &str) -> AgentCandidate {
        AgentCandidate {
            pane_id: pane.to_owned(),
            label: label.to_owned(),
            status: Attention::Idle,
            workspace_label: workspace.to_owned(),
        }
    }

    /// The herdr news that gives `name`'s root exactly these candidates.
    fn agents_of(name: &str, list: Vec<AgentCandidate>) -> Action {
        let mut map = BTreeMap::new();
        map.insert(root(name), list);
        Action::Herdr(HerdrUpdate::Agents(map))
    }

    /// alpha's pile with `f1` turned into the change `f1` is not: a deletion (whose one
    /// hunk is the file) or an addition (which has no baseline to go back to).
    fn alpha_as(change: Change) -> Pile {
        let mut p = pile("alpha");
        p.rows[0].change = change;
        match change {
            Change::Deleted => p.rows[0].current = None,
            Change::Added => p.rows[0].baseline = None,
            _ => {}
        }
        p
    }

    /// `u` needs a hunk to point at. A collapsed row has none on the row itself, so the
    /// restore is the whole file — and a whole file asks first, exactly as `shift-u` does.
    #[test]
    fn app_restore_on_a_hunkless_row_is_the_whole_file() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.apply(pile_event_seq("alpha", 1, alpha_collapsed(Collapsed::Glob)));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);

        let scope = app.restore_scope().expect("a scope on a selected row");
        assert!(
            matches!(scope, RestoreScope::File { hunks: 0, .. }),
            "no hunk to point at: {scope:?}"
        );
        assert_eq!(
            app.handle(Action::Restore),
            (Changed::Yes, None),
            "the whole file asks first"
        );
        assert_eq!(
            app.confirm_restore(),
            Some(&scope),
            "and asks about exactly that scope"
        );

        // An expansion held beside the row does not turn `u` back into a hunk restore: the
        // row still carries no hunks, and `a`/`A` follow the same rule (Phase 6).
        let asked = app.selected_row().expect("f1").clone();
        app.handle(Action::Confirm);
        app.set_expanded(root("alpha"), &asked, expansion_of(3, 0));
        assert_eq!(app.view_hunks().len(), 3, "three hunks are on screen");
        assert!(matches!(
            app.restore_scope(),
            Some(RestoreScope::File { .. })
        ));
    }

    /// The asking rule, both halves on one app: `shift-u` opens the confirm and yields no
    /// effect until `y`; `u` on a hunk starts straight away. A whole file going back is the
    /// bigger surprise, so only it asks (kickoff ruling item 2).
    #[test]
    fn app_restore_file_asks_first_and_restore_hunk_does_not() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.apply(pile_event_seq("alpha", 1, alpha_hunks(3)));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);
        app.handle(Action::HunkNext);

        // The hunk half: no modal, an effect at once, and the hunk the cursor was on.
        let (changed, effect) = app.handle(Action::Restore);
        assert_eq!(changed, Changed::Yes);
        assert!(app.confirm.is_none(), "a hunk restore never asks");
        let Some(Effect::Restore(reqs)) = effect else {
            panic!("a restore effect: {effect:?}");
        };
        assert_eq!(reqs.len(), 1, "a restore is never more than one root");
        assert!(
            matches!(reqs[0].1, RestoreRequest::Hunk { index: 1, .. }),
            "{:?}",
            reqs[0].1
        );
        assert_eq!(status(&app), "restoring…");

        // Nothing else starts while that one is in flight.
        assert_eq!(app.handle(Action::RestoreFile), (Changed::Yes, None));
        assert_eq!(status(&app), RESTORE_IN_PROGRESS);
        app.restored(vec![(root("alpha"), restored_ok(2, alpha_hunks(2)))]);
        assert_eq!(status(&app), "restored f1 hunk 2");

        // The file half: the modal first, `n` drops it without an effect, `y` starts it.
        let (changed, effect) = app.handle(Action::RestoreFile);
        assert_eq!((changed, effect), (Changed::Yes, None), "the modal asks");
        assert!(app.confirm_restore().is_some());
        assert_eq!(app.handle(Action::Cancel), (Changed::Yes, None));
        assert!(app.confirm.is_none(), "n closes it");
        assert!(app.restoring.is_none(), "and starts nothing");

        app.handle(Action::RestoreFile);
        let (_, effect) = app.handle(Action::Confirm);
        let Some(Effect::Restore(reqs)) = effect else {
            panic!("y starts it: {effect:?}");
        };
        assert!(matches!(reqs[0].1, RestoreRequest::File(_)), "{reqs:?}");
    }

    /// R2 (verifier F3): a row the folder never read has nothing to put back, so `shift-u`
    /// on it asks nothing at all and the status line carries the engine's own refusal
    /// rather than a question the restore could not have honoured.
    #[test]
    fn app_restore_of_an_unread_row_asks_nothing_and_says_why() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        let mut p = alpha_collapsed(Collapsed::Unread {
            over_bytes: 512 * 1024,
        });
        p.rows[0].current = None;
        app.apply(pile_event_seq("alpha", 1, p));
        app.select(Some(row("alpha", "f1")));

        let (changed, effect) = app.handle(Action::RestoreFile);
        assert_eq!((changed, effect), (Changed::Yes, None));
        assert!(app.confirm.is_none(), "no question is asked");
        assert!(app.restoring.is_none(), "and nothing starts");
        assert_eq!(status(&app), "f1: not read; restore is not offered");

        // `u` on the same row is the whole file (it has no hunks), and answers the same.
        let (_, effect) = app.handle(Action::Restore);
        assert!(effect.is_none(), "{effect:?}");
        assert!(app.confirm.is_none());
        assert_eq!(status(&app), "f1: not read; restore is not offered");
    }

    /// Restoring a file that is not in the baseline **removes** it, so the question says
    /// so: `Delete f1?`, not `Restore f1?` (kickoff deliverable 9, F16). The status line
    /// afterwards uses the same vocabulary.
    #[test]
    fn app_restore_of_an_added_file_asks_with_the_delete_wording() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.apply(pile_event_seq("alpha", 1, alpha_as(Change::Added)));
        app.select(Some(row("alpha", "f1")));

        app.handle(Action::RestoreFile);
        let scope = app.confirm_restore().expect("it asks").clone();
        assert!(
            matches!(scope, RestoreScope::File { added: true, .. }),
            "{scope:?}"
        );
        assert_eq!(
            restore_question(&scope),
            "Delete f1? (added since baseline)"
        );

        app.handle(Action::Confirm);
        app.restored(vec![(
            root("alpha"),
            restored_ok(2, without(pile("alpha"), &["f1"])),
        )]);
        assert_eq!(status(&app), "removed f1 (added since baseline)");

        // A modified file keeps the plain wording, so the two are told apart by the words.
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::RestoreFile);
        assert_eq!(
            restore_question(app.confirm_restore().expect("it asks")),
            "Restore f1 · 1 hunk?"
        );
    }

    /// Verifier (b) F3: `u` on an **added** file is a deletion, so it asks — the route
    /// `app_restore_of_an_added_file_asks_with_the_delete_wording` never covered, because it
    /// drives `RestoreFile` only. Before the fix `u` here emitted `Effect::Restore(Hunk)`
    /// straight away, the engine took its removal path, and the status line said
    /// `restored f1 hunk 1` about a file that was gone.
    ///
    /// The diff has focus and the cursor is on the row's one content hunk — the worst case,
    /// because that is exactly where a hunk restore would otherwise be right.
    #[test]
    fn app_restore_hunk_on_an_added_file_asks_to_delete() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.apply(pile_event_seq("alpha", 1, alpha_as(Change::Added)));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);
        assert_eq!(app.effective_focus(), Focus::Diff);

        let scope = app.restore_scope().expect("a scope on a selected row");
        assert!(
            matches!(scope, RestoreScope::File { added: true, .. }),
            "the whole file, not its one hunk: {scope:?}"
        );
        assert_eq!(
            restore_question(&scope),
            "Delete f1? (added since baseline)"
        );

        // `u` asks, and writes nothing until the answer.
        let (_, effect) = app.handle(Action::Restore);
        assert!(effect.is_none(), "nothing is removed yet: {effect:?}");
        assert!(app.confirm_restore().is_some(), "the question is on screen");

        // `n` keeps the file.
        let (_, effect) = app.handle(Action::Cancel);
        assert!(effect.is_none(), "{effect:?}");
        assert!(app.confirm_restore().is_none());

        // `u` again, then `y`: one whole-file request, and the removal wording.
        app.handle(Action::Restore);
        let (_, effect) = app.handle(Action::Confirm);
        let Some(Effect::Restore(reqs)) = effect else {
            panic!("y starts it: {effect:?}");
        };
        assert_eq!(reqs.len(), 1);
        assert!(matches!(reqs[0].1, RestoreRequest::File(_)), "{reqs:?}");
        app.restored(vec![(
            root("alpha"),
            restored_ok(2, without(pile("alpha"), &["f1"])),
        )]);
        assert_eq!(status(&app), "removed f1 (added since baseline)");
    }

    /// A deletion row's one hunk is the whole file (F16), so `u` on it is a file restore —
    /// which means it asks, and its question says the file was deleted.
    #[test]
    fn app_restore_on_a_deletion_row_is_the_file_and_asks() {
        let deleted = |app: &mut App| {
            app.handle(Action::Resize(100, 30));
            app.apply(pile_event_seq("alpha", 1, alpha_as(Change::Deleted)));
            app.select(Some(row("alpha", "f1")));
            app.handle(Action::Open);
        };
        let mut app = three_roots();
        deleted(&mut app);
        assert!(
            !app.selected_row().expect("f1").hunks.is_empty(),
            "the row does have a hunk — it is just not one to restore alone"
        );

        assert_eq!(app.handle(Action::Restore), (Changed::Yes, None));
        let scope = app.confirm_restore().expect("u asks here").clone();
        assert!(
            matches!(scope, RestoreScope::File { deleted: true, .. }),
            "{scope:?}"
        );
        assert_eq!(restore_question(&scope), "Restore f1? (deleted)");

        // `shift-u` on the same row is the same scope: the two keys agree on a deletion.
        let mut by_file = three_roots();
        deleted(&mut by_file);
        by_file.handle(Action::RestoreFile);
        assert_eq!(by_file.confirm_restore(), Some(&scope));
    }

    /// Deliverable 7's refusals. `shift-i` opens a *file* at a *line*, so there are three
    /// rows it has nothing to open: a deletion (no file), a symlink or any other non-regular
    /// mode (nothing an editor edits in place, and following it would edit the target
    /// instead), and a path that is not UTF-8 (there is no `&str` to build an argv from
    /// without mangling it). Each answers `not editable` and produces no effect — the
    /// keystroke must never fall through to "open something else".
    ///
    /// The positive case is the integration scene `editor_launch_lands_at_the_right_line`,
    /// which is the only place the argv can actually be seen.
    #[test]
    fn app_edit_external_refuses_rows_with_nothing_to_open() {
        // A row `shift-i` *does* open, so the refusals below are about the row and not
        // about the key being unwired.
        let mut ok = three_roots();
        ok.handle(Action::Resize(100, 30));
        ok.select(Some(row("alpha", "f1")));
        let (changed, effect) = ok.handle(Action::EditExternal);
        assert_eq!(changed, Changed::No, "the screen does not move to open one");
        assert!(
            matches!(effect, Some(Effect::EditExternal { .. })),
            "{effect:?}"
        );
        assert!(ok.edit_target().is_some());

        /// Select `path`, open it, and demand the refusal: no effect, and the status says
        /// so rather than the key quietly doing nothing.
        fn refuses(app: &mut App, path: &[u8], what: &str) {
            app.handle(Action::Resize(100, 30));
            app.select(Some(Selection::Row(root("alpha"), path.to_vec())));
            app.handle(Action::Open);
            assert!(app.edit_target().is_none(), "{what} has nothing to open");
            let (changed, effect) = app.handle(Action::EditExternal);
            assert_eq!(effect, None, "{what}: no editor is spawned");
            assert_eq!(changed, Changed::Yes, "{what}: the status moved");
            assert_eq!(status(app), NOT_EDITABLE, "{what}");
        }

        let mut deletion = three_roots();
        deletion.apply(pile_event_seq("alpha", 1, alpha_as(Change::Deleted)));
        refuses(&mut deletion, b"f1", "a deletion");

        let mut symlink = three_roots();
        let mut p = pile("alpha");
        p.rows[0].current = p.rows[0].current.clone().map(|e| Entry {
            mode: Mode::Symlink,
            ..e
        });
        symlink.apply(pile_event_seq("alpha", 1, p));
        refuses(&mut symlink, b"f1", "a symlink");

        let mut non_utf8 = three_roots();
        let mut p = pile("alpha");
        p.rows[0].path = vec![0xff, 0xfe];
        non_utf8.apply(pile_event_seq("alpha", 1, p));
        refuses(&mut non_utf8, &[0xff, 0xfe], "a non-UTF-8 path");
    }

    // ---- deliverable 8: the inline editor -------------------------------------------------

    /// `i` on the selection: the effect the loop would be handed, with the read unanswered.
    fn edit_open(app: &mut App) -> EditOpen {
        let (changed, effect) = app.handle(Action::Edit);
        assert_eq!(
            changed,
            Changed::No,
            "nothing is drawn to ask the loop for the bytes"
        );
        match effect {
            Some(Effect::EditInline(open)) => open,
            other => panic!("an inline-edit effect, got {other:?}"),
        }
    }

    /// `i` answered with `text` as the row's live bytes; hands back the `EditOpen` the
    /// reducer built so a test can assert on the marks it asked for.
    fn edit_on(app: &mut App, text: &str) -> EditOpen {
        let open = edit_open(app);
        let echo = open.clone();
        assert_eq!(
            app.edit_read(open, Ok(text.as_bytes().to_vec())),
            (Changed::Yes, None)
        );
        assert!(app.editor.is_some(), "the editor opened");
        echo
    }

    /// `n` lines `l0`..`lN`, each terminated — a file long enough to hold marks at line
    /// numbers the assertions can tell apart.
    fn lines_of(n: usize) -> String {
        (0..n).map(|i| format!("l{i}\n")).collect()
    }

    /// A clean save answer: the row is gone from the rescan, which is what the engine
    /// returns when the bytes the editor wrote are the bytes the ledger now blesses.
    fn saved_ok(seq: u64, pile: Pile) -> SaveResult {
        Ok(Saved {
            outcome: lastcall_engine::ops::Outcome::default(),
            seq,
            pile,
        })
    }

    /// A save answer that refused with `refused` — the pile still carries the row.
    fn saved_refusing(seq: u64, pile: Pile, refused: Refused) -> SaveResult {
        Ok(Saved {
            outcome: lastcall_engine::ops::Outcome {
                refused: vec![refused],
                ..Default::default()
            },
            seq,
            pile,
        })
    }

    /// The editor opens where `shift-i` would put `$EDITOR` — the hunk under the diff
    /// cursor, at its first *changed* line — and it marks the lines of **every** pending
    /// hunk in the row, not only the one it entered: the reader is looking at the whole
    /// file now, and the gutter is what tells them where the rest of the agent's work is.
    /// The band is the entered hunk alone.
    #[test]
    fn app_edit_opens_at_the_hunk_line_and_marks_its_lines() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.apply(pile_event_seq(
            "alpha",
            1,
            alpha_ranges(&[(5, 9), (20, 23)]),
        ));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);
        assert_eq!(app.handle(Action::HunkNext).0, Changed::Yes);
        assert_eq!(app.diff.hunk, 1, "the cursor is on the second hunk");

        let open = edit_on(&mut app, &lines_of(40));
        assert_eq!(
            open.line, 21,
            "the second hunk's first changed line, one-based (its `new_range` starts at 20 \
             and its first line is a deletion)"
        );
        assert_eq!(
            open.marks,
            vec![5, 6, 7, 8, 20, 21, 22],
            "both hunks' lines are marked"
        );
        assert_eq!(open.band, Some((20, 22)), "the band is the hunk it entered");

        let ed = app.editor.as_ref().expect("open");
        assert_eq!(ed.root, root("alpha"));
        assert_eq!(ed.rendered.path, b"f1");
        assert_eq!(ed.line_at_open, 21);
        assert_eq!(
            ed.buf.cursor,
            Pos { line: 20, col: 0 },
            "the caret is on the zero-based line the one-based `line` names"
        );
        assert_eq!(
            ed.buf.text(),
            lines_of(40),
            "the whole file is in the buffer"
        );
        assert!(!ed.buf.dirty(), "opening a file changes nothing");
        assert!(!ed.saving && !ed.alarm);
        for i in [5, 8, 20, 22] {
            assert!(ed.marked(i), "line {i} is inside a hunk");
        }
        for i in [4, 9, 19, 23] {
            assert!(!ed.marked(i), "line {i} is not");
        }
        assert!(
            ed.in_band(20) && ed.in_band(22),
            "the entered hunk is tinted"
        );
        assert!(!ed.in_band(5) && !ed.in_band(23), "and nothing else is");

        // From the nav there is no hunk under a cursor, so `i` opens at the row's *first*
        // hunk — the same choice `shift-i` makes, so the two keys never disagree.
        let mut from_nav = three_roots();
        from_nav.handle(Action::Resize(100, 30));
        from_nav.apply(pile_event_seq(
            "alpha",
            1,
            alpha_ranges(&[(5, 9), (20, 23)]),
        ));
        from_nav.select(Some(row("alpha", "f1")));
        assert_eq!(from_nav.effective_focus(), Focus::Nav);
        let open = edit_on(&mut from_nav, &lines_of(40));
        assert_eq!(open.line, 6);
        assert_eq!(open.band, Some((5, 8)));
        assert_eq!(
            open.line,
            from_nav.edit_target().expect("openable").2,
            "`i` and `shift-i` ask for the same line"
        );
    }

    /// Verifier (b) F2: two `i` in one burst used to open two editors, the second read's
    /// answer replacing the buffer the reader had already typed into. Now the second press
    /// asks for nothing while the first read is in flight, and an answer to an open that is
    /// no longer pending — a duplicate, or a read that was already superseded — is dropped.
    #[test]
    fn app_edit_does_not_replace_a_dirty_editor() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.select(Some(row("alpha", "f1")));

        let open = edit_open(&mut app);
        assert_eq!(
            app.edit_pending,
            Some(open.generation),
            "the read is in flight"
        );
        assert_eq!(
            app.handle(Action::Edit),
            (Changed::No, None),
            "a second `i` while the first read is out asks for nothing"
        );
        assert_eq!(
            app.edit_pending,
            Some(open.generation),
            "and does not move the open the answer will be matched against"
        );

        assert_eq!(
            app.edit_read(open.clone(), Ok(lines_of(10).into_bytes())),
            (Changed::Yes, None)
        );
        assert_eq!(app.edit_pending, None, "the open is answered");
        app.handle(Action::Editor(EditorKey::Edit(EditKey::Insert(
            "mine ".to_owned(),
        ))));
        let typed = app.editor.as_ref().expect("open").buf.text();
        assert!(typed.starts_with("mine l0"), "the reader typed: {typed:?}");
        assert!(app.editor.as_ref().expect("open").buf.dirty());

        // The second read's answer — the same file, a fresh copy off the disk — lands.
        assert_eq!(
            app.edit_read(open, Ok(lines_of(10).into_bytes())),
            (Changed::No, None),
            "an answer nobody is waiting for is dropped"
        );
        let ed = app.editor.as_ref().expect("still open");
        assert_eq!(ed.buf.text(), typed, "the typed text survives");
        assert!(ed.buf.dirty(), "and the buffer is still dirty");

        // Belt to that braces: `i` with an editor up (no keymap path reaches it — the
        // editor eats the key as text) opens nothing over the buffer either.
        assert_eq!(app.handle(Action::Edit), (Changed::No, None));
        assert_eq!(
            app.editor.as_ref().expect("still open").buf.text(),
            typed,
            "the buffer is untouched"
        );
    }

    /// Esc on a buffer nobody typed in closes it outright; Esc on a dirty one asks, and the
    /// question names the file. Cancelling puts the reader back in their text with every
    /// character still there — the whole point of asking.
    #[test]
    fn app_edit_dirty_esc_asks_and_clean_esc_closes() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.select(Some(row("alpha", "f1")));

        edit_on(&mut app, &lines_of(10));
        assert_eq!(
            app.handle(Action::Editor(EditorKey::Close)),
            (Changed::Yes, None)
        );
        assert!(app.editor.is_none(), "a clean buffer just closes");
        assert_eq!(app.confirm_discard(), None, "and nothing was asked");

        edit_on(&mut app, &lines_of(10));
        app.handle(Action::Editor(EditorKey::Edit(EditKey::Insert(
            "typed".to_owned(),
        ))));
        assert!(app.editor.as_ref().expect("open").buf.dirty());
        assert_eq!(
            app.handle(Action::Editor(EditorKey::Close)),
            (Changed::Yes, None)
        );
        assert_eq!(app.confirm_discard(), Some(&b"f1"[..]), "it asks, by name");
        assert!(
            app.editor.is_some(),
            "and the buffer is still there to keep"
        );

        assert_eq!(app.handle(Action::Cancel), (Changed::Yes, None));
        assert_eq!(app.confirm_discard(), None);
        let ed = app.editor.as_ref().expect("kept");
        assert!(
            ed.buf.text().starts_with("typedl0\n"),
            "cancelling kept every character: {:?}",
            ed.buf.text()
        );

        // Answering yes is the only path that throws the text away.
        app.handle(Action::Editor(EditorKey::Close));
        assert_eq!(app.confirm_discard(), Some(&b"f1"[..]), "it asks again");
        assert_eq!(app.handle(Action::Confirm), (Changed::Yes, None));
        assert!(app.editor.is_none());
        assert_eq!(app.confirm_discard(), None);
    }

    /// `Ctrl-S` while the engine has not answered yet is ignored — one buffer is never
    /// written twice at once — and a save the engine **refuses** keeps every character:
    /// the file moved under the reader, and discarding their work to tell them so would be
    /// the worst possible reading of "not saved". The header goes red until the next key
    /// and the status spells out the two keys that reload.
    #[test]
    fn app_edit_save_refused_keeps_the_buffer() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.select(Some(row("alpha", "f1")));
        edit_on(&mut app, &lines_of(4));
        app.handle(Action::Editor(EditorKey::Edit(EditKey::Insert(
            "mine ".to_owned(),
        ))));

        let (changed, effect) = app.handle(Action::Editor(EditorKey::Save));
        assert_eq!(changed, Changed::Yes);
        let Some(Effect::Save {
            root: r,
            rendered,
            bytes,
        }) = effect
        else {
            panic!("a save effect, got {effect:?}");
        };
        assert_eq!(r, root("alpha"));
        assert_eq!(rendered.path, b"f1");
        assert_eq!(
            String::from_utf8(bytes).expect("utf-8"),
            "mine l0\nl1\nl2\nl3\n",
            "the bytes are the buffer, verbatim"
        );
        assert!(app.editor.as_ref().expect("open").saving);
        assert_eq!(
            app.handle(Action::Editor(EditorKey::Save)),
            (Changed::No, None),
            "a second Ctrl-S while the first is in flight writes nothing"
        );

        let refused = Refused::Moved {
            path: b"f1".to_vec(),
            live: None,
        };
        assert_eq!(
            app.saved(
                root("alpha"),
                b"f1".to_vec(),
                saved_refusing(2, pile("alpha"), refused)
            ),
            Changed::Yes
        );
        let ed = app.editor.as_ref().expect("the buffer is kept");
        assert_eq!(ed.buf.text(), "mine l0\nl1\nl2\nl3\n");
        assert!(ed.buf.dirty(), "still unsaved, and it says so");
        assert!(ed.alarm, "the header is red");
        assert!(!ed.saving, "and Ctrl-S works again");
        assert_eq!(
            status(&app),
            "f1: changed since you opened it; not saved — Esc, then i to reload"
        );

        // Any key means the reader has seen the red header.
        app.handle(Action::Editor(EditorKey::Edit(EditKey::Right)));
        assert!(!app.editor.as_ref().expect("open").alarm);
    }

    /// Typing above a mark moves it: the mark describes the agent's line, not the line
    /// number it happened to have when the editor opened, so a newline inserted above it
    /// has to carry it down or the gutter starts lying.
    #[test]
    fn app_edit_marks_follow_insertions_above_them() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.apply(pile_event_seq(
            "alpha",
            1,
            alpha_ranges(&[(5, 9), (20, 23)]),
        ));
        app.select(Some(row("alpha", "f1")));
        edit_on(&mut app, &lines_of(40));
        assert_eq!(app.editor.as_ref().expect("open").band, Some((5, 8)));

        // Put the caret on line 0 — above everything — and break the line there.
        app.handle(Action::Editor(EditorKey::Click(0, 0)));
        assert_eq!(app.editor.as_ref().expect("open").buf.cursor.line, 0);
        app.handle(Action::Editor(EditorKey::Edit(EditKey::Newline)));

        let ed = app.editor.as_ref().expect("open");
        assert_eq!(ed.buf.line_count(), 41, "one line longer");
        assert_eq!(
            ed.marks,
            vec![6, 7, 8, 9, 21, 22, 23],
            "every mark moved down by the one line inserted above it"
        );
        assert_eq!(ed.band, Some((6, 9)), "and so did the band");

        // A deletion above them carries them back.
        app.handle(Action::Editor(EditorKey::Click(0, 0)));
        app.handle(Action::Editor(EditorKey::Edit(EditKey::Delete)));
        let ed = app.editor.as_ref().expect("open");
        assert_eq!(ed.buf.line_count(), 40);
        assert_eq!(ed.marks, vec![5, 6, 7, 8, 20, 21, 22]);
        assert_eq!(ed.band, Some((5, 8)));
    }

    /// Typing *inside* the band grows it. Splitting the hunk's last line leaves both halves
    /// inside the hunk the reader entered, so the tint has to cover both — which is the one
    /// place the band and a bare mark part company (`>=` at the end, not `>`).
    #[test]
    fn app_edit_band_follows_insertions_inside_it() {
        let open_at_band = |lines: &str| {
            let mut app = three_roots();
            app.handle(Action::Resize(100, 30));
            app.apply(pile_event_seq("alpha", 1, alpha_ranges(&[(5, 9)])));
            app.select(Some(row("alpha", "f1")));
            edit_on(&mut app, lines);
            assert_eq!(app.editor.as_ref().expect("open").band, Some((5, 8)));
            app
        };

        // The band's last line, split in two: the band ends one lower.
        let mut app = open_at_band(&lines_of(20));
        app.handle(Action::Editor(EditorKey::Click(
            8 - app.editor.as_ref().unwrap().buf.top as u16,
            1,
        )));
        assert_eq!(app.editor.as_ref().expect("open").buf.cursor.line, 8);
        app.handle(Action::Editor(EditorKey::Edit(EditKey::Newline)));
        let ed = app.editor.as_ref().expect("open");
        assert_eq!(ed.band, Some((5, 9)), "the band grew with the split");
        assert!(ed.marked(9), "and the new line is inside the hunk too");

        // A line inserted in the middle of the band grows it the same way.
        let mut app = open_at_band(&lines_of(20));
        app.handle(Action::Editor(EditorKey::Click(6, 0)));
        assert_eq!(app.editor.as_ref().expect("open").buf.cursor.line, 6);
        app.handle(Action::Editor(EditorKey::Edit(EditKey::Newline)));
        let ed = app.editor.as_ref().expect("open");
        assert_eq!(ed.band, Some((5, 9)));
        assert_eq!(ed.marks, vec![5, 6, 7, 8, 9]);
    }

    /// The engine's refusals, scripted: a row that will not go in a buffer says **why** and
    /// names the key that opens it anyway, because "no" is only half an answer when there
    /// is a second way in. Every other refusal is the compare-and-swap speaking, in the
    /// vocabulary every other refused op uses. In none of them does an editor open.
    #[test]
    fn app_edit_refuses_binary_and_oversize_rows() {
        let refuses = |result: Result<Vec<u8>, Refused>, expected: &str| {
            let mut app = three_roots();
            app.handle(Action::Resize(100, 30));
            app.select(Some(row("alpha", "f1")));
            let open = edit_open(&mut app);
            assert_eq!(app.edit_read(open, result), (Changed::Yes, None));
            assert!(app.editor.is_none(), "no editor opened: {expected}");
            assert_eq!(status(&app), expected);
        };

        let not_editable = |why: &str| {
            Err(Refused::NotEditable {
                path: b"f1".to_vec(),
                why: why.to_owned(),
            })
        };
        refuses(not_editable("binary"), "use shift-i: binary");
        refuses(not_editable("over 512 KiB"), "use shift-i: over 512 KiB");
        refuses(
            not_editable("the file is gone"),
            "use shift-i: the file is gone",
        );
        refuses(
            Err(Refused::Moved {
                path: b"f1".to_vec(),
                live: None,
            }),
            "f1: changed since rendered; not opened",
        );
        refuses(
            Err(Refused::Unhashable {
                path: b"f1".to_vec(),
                reason: "permission denied".to_owned(),
            }),
            "f1: cannot hash (permission denied); not opened",
        );
        // Belt to the engine's braces: bytes that are not text never reach a `String`.
        refuses(Ok(vec![0x61, 0xff, 0x0a]), "use shift-i: binary");
    }

    /// A save that never reached the ledger keeps the buffer for the same reason a refused
    /// one does, and names the root so the reader knows which engine spoke.
    #[test]
    fn app_edit_save_error_keeps_the_buffer() {
        let after = |result: SaveResult, expected: &str| {
            let mut app = three_roots();
            app.handle(Action::Resize(100, 30));
            app.select(Some(row("alpha", "f1")));
            edit_on(&mut app, &lines_of(4));
            app.handle(Action::Editor(EditorKey::Edit(EditKey::Insert(
                "mine ".to_owned(),
            ))));
            app.handle(Action::Editor(EditorKey::Save));
            assert_eq!(
                app.saved(root("alpha"), b"f1".to_vec(), result),
                Changed::Yes
            );
            let ed = app.editor.as_ref().expect("the buffer is kept");
            assert_eq!(ed.buf.text(), "mine l0\nl1\nl2\nl3\n");
            assert!(!ed.saving, "Ctrl-S works again");
            assert!(!ed.alarm, "an error is not the file moving under them");
            assert_eq!(status(&app), expected);
        };

        after(
            Err(AcceptFailed::LedgerBusy),
            "ledger busy in alpha — try again",
        );
        after(
            Err(AcceptFailed::Other("disk full".to_owned())),
            "alpha: disk full",
        );
        after(
            saved_refusing(
                2,
                pile("alpha"),
                Refused::NotRoundTrippable {
                    path: b"f1".to_vec(),
                },
            ),
            "f1: eol conversion is not round-trippable; not saved",
        );
    }

    /// A clean save is the whole point of the phase: the rescan has the row gone, the
    /// editor closes, and the §6.7 advance moves the selection off the row exactly as an
    /// accept would.
    #[test]
    fn app_edit_save_closes_the_editor_and_advances() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.select(Some(row("alpha", "f1")));
        edit_on(&mut app, &lines_of(4));
        app.handle(Action::Editor(EditorKey::Edit(EditKey::Insert(
            "mine ".to_owned(),
        ))));
        app.handle(Action::Editor(EditorKey::Save));

        let rescan = without(pile("alpha"), &["f1"]);
        assert_eq!(
            app.saved(root("alpha"), b"f1".to_vec(), saved_ok(2, rescan)),
            Changed::Yes
        );
        assert!(app.editor.is_none(), "the editor closed");
        assert_eq!(status(&app), "saved f1");
        assert_eq!(
            app.selection,
            Some(row("alpha", "f2")),
            "the selection moved off the row that is gone"
        );
    }

    /// A bracketed paste lands as one edit and comes back out byte for byte — tabs, both
    /// line endings, and all — because a paste that is silently reformatted is a paste that
    /// corrupted the reader's file.
    #[test]
    fn app_edit_paste_inserts_verbatim() {
        let km = Keymap::defaults();
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.select(Some(row("alpha", "f1")));
        edit_on(&mut app, "a\nb\n");

        let pasted = "one\r\n\ttwo\nthree";
        let action = editor_action(&Event::Paste(pasted.to_owned()), &km, app.enhanced)
            .expect("a paste is handled");
        assert_eq!(
            action,
            Action::Editor(EditorKey::Edit(EditKey::Insert(pasted.to_owned()))),
            "one event, one edit"
        );
        assert_eq!(app.handle(action), (Changed::Yes, None));

        let ed = app.editor.as_ref().expect("open");
        assert_eq!(
            ed.buf.text(),
            "one\n\ttwo\nthreea\nb\n",
            "every character is in the file — the tab, the text and the split — with the \
             CRLF taken to the buffer's own ending (verifier (a) F3), which is the one \
             thing a paste may not carry into a file that never used it"
        );
        assert_eq!(
            ed.buf.cursor,
            Pos { line: 2, col: 5 },
            "and the caret is at the end of what was pasted"
        );

        // The save carries the same bytes the buffer shows.
        let (_, effect) = app.handle(Action::Editor(EditorKey::Save));
        let Some(Effect::Save { bytes, .. }) = effect else {
            panic!("a save effect, got {effect:?}");
        };
        assert_eq!(bytes, b"one\n\ttwo\nthreea\nb\n");

        // The same paste into a file that *is* CRLF keeps its CRLF: the rule is the file's
        // ending, not a preference for `\n`.
        let mut crlf = three_roots();
        crlf.handle(Action::Resize(100, 30));
        crlf.select(Some(row("alpha", "f1")));
        edit_on(&mut crlf, "a\r\nb\r\n");
        crlf.handle(Action::Editor(EditorKey::Edit(EditKey::Insert(
            pasted.to_owned(),
        ))));
        assert_eq!(
            crlf.editor.as_ref().expect("open").buf.text(),
            "one\r\n\ttwo\r\nthreea\r\nb\r\n"
        );
    }

    /// The reducer owns the scroll (F19): the renderer is handed a `&App` and may not move
    /// what it is drawing, so a `Resize` has to re-clamp the buffer's window itself or the
    /// caret ends up off the screen it is being drawn on.
    #[test]
    fn app_edit_resize_reclamps_the_viewport() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 40));
        app.apply(pile_event_seq("alpha", 1, alpha_ranges(&[(120, 124)])));
        app.select(Some(row("alpha", "f1")));
        edit_on(&mut app, &lines_of(200));

        let inside = |app: &App| {
            let ed = app.editor.as_ref().expect("open");
            let rows = app.page_rows();
            assert!(
                ed.buf.top <= ed.buf.cursor.line && ed.buf.cursor.line < ed.buf.top + rows,
                "caret {} is outside the {rows}-row window at {}",
                ed.buf.cursor.line,
                ed.buf.top
            );
            (ed.buf.top, rows)
        };
        let (tall_top, tall_rows) = inside(&app);
        assert_eq!(tall_rows, 36, "40 rows less the header, status and borders");
        assert_eq!(app.editor.as_ref().expect("open").buf.cursor.line, 120);

        // Shrink the frame: the same caret has to be inside a much smaller window.
        assert_eq!(app.handle(Action::Resize(100, 12)), (Changed::Yes, None));
        let (short_top, short_rows) = inside(&app);
        assert_eq!(short_rows, 8);
        assert!(
            short_top > tall_top,
            "the window scrolled down to keep the caret: {tall_top} -> {short_top}"
        );

        // Growing it again keeps the caret on screen too.
        app.handle(Action::Resize(100, 40));
        inside(&app);
    }

    /// `m` flags what is on screen: from the nav there is no hunk under a cursor, so the
    /// flag is the file; from the diff it is the hunk the cursor is on, captured with the
    /// header and body the reader was looking at.
    #[test]
    fn app_flag_from_the_nav_carries_no_hunk_and_from_the_diff_carries_the_cursor_hunk() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.apply(pile_event_seq("alpha", 1, alpha_hunks(3)));
        app.select(Some(row("alpha", "f1")));
        assert_eq!(app.effective_focus(), Focus::Nav);

        let (changed, effect) = app.handle(Action::Flag);
        assert_eq!((changed, effect), (Changed::Yes, None), "the note opens");
        let target = app.note.as_ref().expect("open").target.clone();
        assert!(matches!(target, FlagTarget::File { .. }), "{target:?}");
        assert_eq!(target.rendered_hunk(), None, "the file, not a hunk");
        assert_eq!(target.label(), "f1 · whole file");
        assert_eq!(target.modal_title(), " flag whole file ");

        type_note(&mut app, "the whole rewrite needs another look");
        let row_now = app.roots[&root("alpha")]
            .row(b"f1")
            .expect("the row")
            .clone();
        let (_, effect) = app.handle(Action::Note(NoteKey::Send));
        assert_eq!(
            effect,
            Some(Effect::Flag {
                root: root("alpha"),
                path: b"f1".to_vec(),
                note: "the whole rewrite needs another look".to_owned(),
                hunk: None,
                // Ruling P4: with no diff to quote, the export carries the row's shape.
                summary: Some(FlagSummary {
                    hunks: 3,
                    added: row_now.added,
                    deleted: row_now.deleted,
                }),
                label: "f1".to_owned(),
            })
        );

        // The same row from the diff, cursor on the third hunk.
        app.handle(Action::Open);
        app.handle(Action::HunkNext);
        app.handle(Action::HunkNext);
        assert_eq!(app.diff.hunk, 2);
        app.handle(Action::Flag);
        let target = app.note.as_ref().expect("open").target.clone();
        assert_eq!(target.label(), "f1 · hunk 3 of 3");
        assert_eq!(target.modal_title(), " flag hunk 3 of 3 ");
        assert_eq!(
            target.summary(),
            None,
            "a hunk flag quotes its lines instead"
        );
        let rendered = target.rendered_hunk().expect("a hunk flag");
        assert_eq!(rendered.of, 3, "content hunks, as the screen counted them");
        assert_eq!(rendered.hunk.index, 2);
        assert_eq!(
            rendered.hunk.header,
            hunk_header(&app.view_hunks()[2]),
            "the header the reader was looking at"
        );
    }

    /// Verifier (a) F2: a collapsed row nobody expanded has no hunks the scan counted, so a
    /// whole-file flag on it carries **no** summary rather than one claiming `0 hunks`. On a
    /// Binary row the line counts are not even line counts of a diff. Expanding a Glob row
    /// gives the counts back, because then there is something on screen to count.
    #[test]
    fn app_flag_on_a_collapsed_row_carries_no_summary() {
        for kind in [Collapsed::Glob, Collapsed::Binary] {
            let mut app = three_roots();
            app.handle(Action::Resize(100, 30));
            app.apply(pile_event_seq("alpha", 1, alpha_collapsed(kind)));
            app.select(Some(row("alpha", "f1")));

            app.handle(Action::Flag);
            let target = app.note.as_ref().expect("open").target.clone();
            assert_eq!(target.label(), "f1 · whole file", "{kind:?}");
            assert_eq!(
                target.summary(),
                None,
                "{kind:?}: nothing was diffed, so nothing is claimed"
            );

            // …and the flag the engine seam receives carries the absence too.
            let (_, effect) = app.handle(Action::Note(NoteKey::Send));
            let Some(Effect::Flag { summary, hunk, .. }) = effect else {
                panic!("{kind:?}: Enter sends: {effect:?}");
            };
            assert_eq!(summary, None, "{kind:?}");
            assert_eq!(hunk, None, "{kind:?}: a whole-file flag");
        }

        // Expanded, the Glob row has hunks on screen and the summary comes back.
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.apply(pile_event_seq("alpha", 1, alpha_collapsed(Collapsed::Glob)));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Expand);
        let asked = app.selected_row().expect("f1").clone();
        app.set_expanded(root("alpha"), &asked, expansion_of(2, 0));
        assert_eq!(app.view_hunks().len(), 2, "the expansion is on screen");

        app.handle(Action::Flag);
        let summary = app
            .note
            .as_ref()
            .expect("open")
            .target
            .summary()
            .expect("an expanded row counts");
        assert_eq!(summary.hunks, 2, "the hunks the reader can see");
    }

    /// F14: the target is captured when `m` is pressed. A pile that lands while the note is
    /// open — an agent still writing — may reorder or remove hunks; the flag must not move
    /// onto a different one, and must still be written when the reader presses Enter.
    #[test]
    fn app_note_modal_keeps_the_hunk_it_opened_on_across_a_pile() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.apply(pile_event_seq("alpha", 1, alpha_hunks(3)));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);
        app.handle(Action::HunkNext);
        app.handle(Action::HunkNext);
        app.handle(Action::Flag);
        let opened = app.note.as_ref().expect("open").target.clone();
        assert_eq!(opened.label(), "f1 · hunk 3 of 3");

        // The agent rewrites the file: one hunk left, and the cursor is clamped onto it.
        app.apply(pile_event_seq("alpha", 2, alpha_hunks(1)));
        assert_eq!(
            app.note.as_ref().expect("still open").target,
            opened,
            "the pile does not move the flag"
        );
        assert_eq!(app.view_hunks().len(), 1, "the screen did move on");

        type_note(&mut app, "look at this");
        let (_, effect) = app.handle(Action::Note(NoteKey::Send));
        let Some(Effect::Flag { hunk, .. }) = effect else {
            panic!("a flag effect: {effect:?}");
        };
        let hunk = hunk.expect("the hunk it opened on");
        assert_eq!(
            (hunk.hunk.index, hunk.of),
            (2, 3),
            "as captured, not as now"
        );
    }

    /// Verifier (b) F5: `e` then `m` on hunk 2 of 3 quotes **that** hunk.
    ///
    /// Accept and restore stay whole-row on a collapsed row — a collapsed row is one
    /// accept (§6.3) and the expansion's line cap makes a partial restore unsound — but
    /// quoting an expansion hunk writes nothing and is exactly what the reader who pressed
    /// `e` is asking about.
    #[test]
    fn app_flag_on_an_expansion_hunk_carries_that_hunk() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.apply(pile_event_seq("alpha", 1, alpha_collapsed(Collapsed::Glob)));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);
        let asked = app.selected_row().expect("f1").clone();
        app.set_expanded(root("alpha"), &asked, expansion_of(3, 0));
        app.handle(Action::HunkNext);
        assert_eq!(app.diff.hunk, 1, "the cursor is on hunk 2 of 3");

        let target = app.flag_target().expect("a target");
        let FlagTarget::Hunk { hunk, of, .. } = &target else {
            panic!("the hunk under the cursor, not the file: {target:?}");
        };
        assert_eq!((hunk.index, *of), (1, 3));
        assert_eq!(target.label(), "f1 · hunk 2 of 3");

        // …and the write carries it, so the export quotes that hunk and counts `of 3`.
        app.handle(Action::Flag);
        type_note(&mut app, "look at this");
        let (_, effect) = app.handle(Action::Note(NoteKey::Send));
        let Some(Effect::Flag { hunk, label, .. }) = effect else {
            panic!("a flag effect: {effect:?}");
        };
        let hunk = hunk.expect("the expansion hunk, not the file");
        assert_eq!((hunk.hunk.index, hunk.of), (1, 3));
        assert_eq!(label, "f1 hunk 2");

        // Accept and restore are untouched: the row is still one of each.
        assert!(
            matches!(
                app.accept_scope(),
                Some(AcceptAnswer::Take(AcceptScope::File { .. }))
            ),
            "{:?}",
            app.accept_scope()
        );
        assert!(
            matches!(app.restore_scope(), Some(RestoreScope::File { .. })),
            "{:?}",
            app.restore_scope()
        );
    }

    /// One agent under the root: there is nothing to ask, so the export is staged and the
    /// status line names the agent it went to.
    #[test]
    fn app_flagged_with_one_agent_stages_without_asking() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.handle(agents_of(
            "alpha",
            vec![agent("w1:p1", "claude", "lastcall")],
        ));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Flag);
        type_note(&mut app, "why?");
        let (_, flag) = app.handle(Action::Note(NoteKey::Send));
        assert!(
            matches!(flag, Some(Effect::Flag { ref label, .. }) if label == "f1"),
            "the write carries its own words: {flag:?}"
        );

        let (changed, effect) = app.flagged(
            root("alpha"),
            flag_of("f1"),
            flagged_ok("EXPORT", 1, pile("alpha")),
        );
        assert_eq!(changed, Changed::Yes);
        assert!(app.picker.is_none(), "one candidate is not a question");
        assert_eq!(
            effect,
            Some(Effect::Stage {
                pane_id: "w1:p1".to_owned(),
                flag: "f1".to_owned(),
                export: "EXPORT".to_owned(),
            })
        );
        assert_eq!(status(&app), "flagged f1 · staged to claude");
    }

    /// Two agents: lastcall never picks for the reader, so the picker opens and the export
    /// is held until one is chosen. Esc drops the send and keeps the flag.
    #[test]
    fn app_flagged_with_two_agents_opens_the_picker() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.handle(agents_of(
            "alpha",
            vec![
                agent("w1:p1", "claude", "lastcall"),
                agent("w2:p3", "codex", "spike"),
            ],
        ));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Flag);
        type_note(&mut app, "look at this");
        app.handle(Action::Note(NoteKey::Send));

        let (changed, effect) = app.flagged(
            root("alpha"),
            flag_of("f1"),
            flagged_ok("EXPORT", 1, pile("alpha")),
        );
        assert_eq!((changed, effect), (Changed::Yes, None), "it asks, silently");
        let picker = app.picker.as_ref().expect("the picker is open");
        assert_eq!(picker.candidates.len(), 2);
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.label, "f1");

        // Esc drops the send and says so; the flag itself is already on disk.
        let mut cancelled = app.clone();
        assert_eq!(
            cancelled.handle(Action::Pick(PickKey::Cancel)),
            (Changed::Yes, None)
        );
        assert!(cancelled.picker.is_none());
        assert_eq!(status(&cancelled), "flagged f1 · not sent");

        // Down then Enter sends to the second pane.
        app.handle(Action::Pick(PickKey::Down));
        let (_, effect) = app.handle(Action::Pick(PickKey::Send));
        assert_eq!(
            effect,
            Some(Effect::Stage {
                pane_id: "w2:p3".to_owned(),
                flag: "f1".to_owned(),
                export: "EXPORT".to_owned(),
            })
        );
        assert_eq!(status(&app), "flagged f1 · staged to codex");
    }

    /// No agent under this root: the export goes to the fallback file under the state dir —
    /// the one file the TUI writes — and the status names it.
    #[test]
    fn app_flagged_with_no_link_writes_the_export_file() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        // An agent under another root is not a candidate for this one.
        app.handle(agents_of(
            "beta",
            vec![agent("w1:p1", "claude", "lastcall")],
        ));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Flag);
        type_note(&mut app, "look at this");
        app.handle(Action::Note(NoteKey::Send));

        let (changed, effect) = app.flagged(
            root("alpha"),
            flag_of("f1"),
            flagged_ok("EXPORT", 1, pile("alpha")),
        );
        assert_eq!(changed, Changed::Yes);
        assert!(app.picker.is_none());
        assert_eq!(
            effect,
            Some(Effect::Export {
                root: root("alpha"),
                label: "f1".to_owned(),
                export: "EXPORT".to_owned(),
            }),
            "the flag's words travel with the write"
        );

        let out = PathBuf::from("/S/exports/alpha/2026-09-05.md");
        assert_eq!(app.exported("f1".to_owned(), Ok(out.clone())), Changed::Yes);
        assert_eq!(
            status(&app),
            format!("flagged f1 · export → {}", out.display())
        );

        // A write that failed says which flag it was and why.
        app.handle(Action::Flag);
        type_note(&mut app, "look at this");
        app.handle(Action::Note(NoteKey::Send));
        app.flagged(
            root("alpha"),
            flag_of("f1"),
            flagged_ok("EXPORT", 2, pile("alpha")),
        );
        app.exported("f1".to_owned(), Err("permission denied".to_owned()));
        assert_eq!(
            status(&app),
            "flagged f1 · export failed: permission denied"
        );
    }

    /// A send that did not land loses nothing: the flag is in the ledger either way, so the
    /// status names the flag **and** the reason rather than reporting a lost note.
    #[test]
    fn app_stage_failure_keeps_the_flag_and_names_the_reason() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.handle(agents_of(
            "alpha",
            vec![agent("w1:p1", "claude", "lastcall")],
        ));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Flag);
        type_note(&mut app, "look at this");
        app.handle(Action::Note(NoteKey::Send));
        app.flagged(
            root("alpha"),
            flag_of("f1"),
            flagged_ok("EXPORT", 1, pile("alpha")),
        );
        assert_eq!(status(&app), "flagged f1 · staged to claude");

        assert_eq!(
            app.staged("f1".to_owned(), Err("pane is gone".to_owned())),
            Changed::Yes
        );
        assert_eq!(status(&app), "flagged f1 · send failed: pane is gone");
        assert!(
            app.roots[&root("alpha")].row(b"f1").is_some(),
            "the row is still pending: a flag does not accept it"
        );

        // A send that landed adds nothing: the optimistic line is already on screen.
        app.handle(Action::Flag);
        type_note(&mut app, "look at this");
        app.handle(Action::Note(NoteKey::Send));
        app.flagged(
            root("alpha"),
            flag_of("f1"),
            flagged_ok("EXPORT", 2, pile("alpha")),
        );
        assert_eq!(app.staged("f1".to_owned(), Ok(())), Changed::No);
        assert_eq!(status(&app), "flagged f1 · staged to claude");
    }

    /// Verifier (b) F2, probe 1. A stage is in flight (the socket is slow) when the
    /// reviewer flags a second row. The stage's answer must not consume the second
    /// flag's identity: `f2` was flagged, not cleared, and its export still has to go
    /// somewhere.
    #[test]
    fn app_flag_answer_during_a_stage_in_flight_is_not_reported_as_cleared() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.handle(agents_of(
            "alpha",
            vec![agent("w1:p1", "claude", "lastcall")],
        ));

        // f1 is flagged and staged; `pane.send_text` has not answered yet.
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Flag);
        type_note(&mut app, "look at this");
        app.handle(Action::Note(NoteKey::Send));
        let (_, effect) = app.flagged(
            root("alpha"),
            flag_of("f1"),
            flagged_ok("EXPORT-F1", 1, pile("alpha")),
        );
        assert!(matches!(effect, Some(Effect::Stage { .. })), "{effect:?}");

        // The reviewer does not wait for the socket: f2 gets its own note.
        app.select(Some(row("alpha", "f2")));
        app.handle(Action::Flag);
        type_note(&mut app, "look at this");
        app.handle(Action::Note(NoteKey::Send));

        // f1's send lands, then f2's ledger write.
        assert_eq!(app.staged("f1".to_owned(), Ok(())), Changed::No);
        let (_, effect) = app.flagged(
            root("alpha"),
            flag_of("f2"),
            flagged_ok("EXPORT-F2", 2, pile("alpha")),
        );
        assert_eq!(
            effect,
            Some(Effect::Stage {
                pane_id: "w1:p1".to_owned(),
                flag: "f2".to_owned(),
                export: "EXPORT-F2".to_owned(),
            }),
            "f2's export is staged, not swallowed"
        );
        assert_eq!(status(&app), "flagged f2 · staged to claude");
    }

    /// Verifier (b) F2, probe 2. Standalone: an export is in flight when the reviewer
    /// flags a second row. Each answer carries its own label, so the file that was
    /// written is named by the flag that was in it.
    #[test]
    fn app_flag_answer_during_an_export_in_flight_keeps_its_own_label() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Flag);
        type_note(&mut app, "look at this");
        app.handle(Action::Note(NoteKey::Send));
        let (_, effect) = app.flagged(
            root("alpha"),
            flag_of("f1"),
            flagged_ok("EXPORT-F1", 1, pile("alpha")),
        );
        assert!(matches!(effect, Some(Effect::Export { .. })), "{effect:?}");

        app.select(Some(row("alpha", "f2")));
        app.handle(Action::Flag);
        type_note(&mut app, "look at this");
        app.handle(Action::Note(NoteKey::Send));

        let out = PathBuf::from("/S/exports/alpha/2026-09-05.md");
        app.exported("f1".to_owned(), Ok(out.clone()));
        assert_eq!(
            status(&app),
            format!("flagged f1 · export → {}", out.display()),
            "the export answer names the flag whose text it wrote"
        );

        let (_, effect) = app.flagged(
            root("alpha"),
            flag_of("f2"),
            flagged_ok("EXPORT-F2", 2, pile("alpha")),
        );
        assert_eq!(
            effect,
            Some(Effect::Export {
                root: root("alpha"),
                label: "f2".to_owned(),
                export: "EXPORT-F2".to_owned(),
            }),
            "f2's export is written too"
        );
    }

    /// Verifier (b) F2, probe 3. `m` then `shift-m` on the same row: two independent
    /// blocking tasks, and the engine's mutex does not order them. The unflag's answer
    /// arrives first and must be read as an unflag — never as the flag's send, which
    /// would append an empty entry to the day's export file.
    #[test]
    fn app_unflag_answer_before_the_flag_answer_writes_no_blank_export() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Flag);
        type_note(&mut app, "look at this");
        app.handle(Action::Note(NoteKey::Send));
        let (_, unflag) = app.handle(Action::Unflag);
        assert_eq!(
            unflag,
            Some(Effect::Unflag {
                root: root("alpha"),
                path: b"f1".to_vec(),
            })
        );

        // The unflag's ledger write finishes first; its export is empty by construction.
        let (_, effect) = app.flagged(
            root("alpha"),
            FlagKind::Unflag,
            flagged_ok("", 2, pile("alpha")),
        );
        assert_eq!(effect, None, "an unflag has nothing to send");
        assert_eq!(status(&app), "flags cleared");

        // The flag's own answer still goes where it was always going.
        let (_, effect) = app.flagged(
            root("alpha"),
            flag_of("f1"),
            flagged_ok("EXPORT-F1", 3, pile("alpha")),
        );
        assert_eq!(
            effect,
            Some(Effect::Export {
                root: root("alpha"),
                label: "f1".to_owned(),
                export: "EXPORT-F1".to_owned(),
            })
        );
        app.exported("f1".to_owned(), Ok(PathBuf::from("/S/x.md")));
        assert_eq!(status(&app), "flagged f1 · export → /S/x.md");
    }

    /// A key event as the terminal reports it.
    fn note_key_event(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(code, modifiers))
    }

    fn note_char(c: char) -> Event {
        note_key_event(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// Type `text` into the open note modal as one insert (what a paste does).
    fn type_note(app: &mut App, text: &str) {
        app.handle(Action::Note(NoteKey::Edit(EditKey::Insert(
            text.to_owned(),
        ))));
    }

    /// Open the note modal on `alpha`'s `f1` with nothing typed yet.
    fn note_open() -> App {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Flag);
        app
    }

    /// One key event through the modal's own mapping and into the app.
    fn note_feed(app: &mut App, event: &Event) -> (Changed, Option<Effect>) {
        let action =
            note_action(event, &Keymap::defaults(), app.enhanced).expect("the modal answers it");
        app.handle(action)
    }

    /// An **empty** note sends: the flag is the message (Phase 7 kickoff deliverable 10, as
    /// ratified). Phase 8 briefly refused it; verifier (a) decision (7) caught that as a
    /// regression against ratified behaviour and this test pins the ratified shape so the
    /// next reader does not re-derive the refusal from first principles.
    #[test]
    fn app_note_modal_empty_note_sends() {
        let mut app = note_open();
        assert_eq!(
            app.note.as_ref().expect("open").text(),
            "",
            "nothing typed yet"
        );

        let (changed, effect) = app.handle(Action::Note(NoteKey::Send));
        assert_eq!(changed, Changed::Yes, "the send redraws");
        assert!(app.note.is_none(), "the modal closes on the send");
        let Some(Effect::Flag { note, path, .. }) = effect else {
            panic!("an empty note is still a flag: {effect:?}");
        };
        assert_eq!(note, "", "the flag carries the empty note verbatim");
        assert_eq!(path, b"f1".to_vec());
    }

    /// The note modal's line discipline. Enter sends; the bindings a terminal reports for a
    /// deliberate line break (`Ctrl-J` everywhere, `Alt-Enter` where Alt is reported) break
    /// the line instead; Esc closes it and writes nothing.
    #[test]
    fn app_note_modal_enter_sends_ctrl_j_and_alt_enter_break_the_line_esc_cancels() {
        let enter = note_key_event(KeyCode::Enter, KeyModifiers::NONE);

        for newline in [
            note_key_event(KeyCode::Char('j'), KeyModifiers::CONTROL),
            note_key_event(KeyCode::Enter, KeyModifiers::ALT),
            // Verifier (a) F5: chat UIs read `Ctrl-Enter` as a line break, and where the
            // terminal cannot tell it from `Enter` this arm is simply never reached.
            note_key_event(KeyCode::Enter, KeyModifiers::CONTROL),
        ] {
            let mut app = note_open();
            for c in "one".chars() {
                note_feed(&mut app, &note_char(c));
            }
            assert_eq!(note_feed(&mut app, &newline), (Changed::Yes, None));
            note_feed(&mut app, &note_char('2'));
            assert_eq!(app.note.as_ref().expect("open").text(), "one\n2");

            let (_, effect) = note_feed(&mut app, &enter);
            assert!(app.note.is_none(), "Enter closes it");
            let Some(Effect::Flag { note, .. }) = effect else {
                panic!("Enter sends: {effect:?}");
            };
            assert_eq!(note, "one\n2", "both lines, as typed");
        }

        // Backspace walks back a character at a time; Esc throws the lot away. `^H` is
        // Backspace too (verifier (a) F4): crossterm reports the byte 0x08 as ctrl-h, and a
        // terminal set to send it for its Backspace key must not lose the key.
        for backspace in [
            note_key_event(KeyCode::Backspace, KeyModifiers::NONE),
            note_key_event(KeyCode::Char('h'), KeyModifiers::CONTROL),
        ] {
            let mut app = note_open();
            type_note(&mut app, "xy");
            assert_eq!(note_feed(&mut app, &backspace), (Changed::Yes, None));
            assert_eq!(app.note.as_ref().expect("open").text(), "x");
        }
        let backspace = note_key_event(KeyCode::Backspace, KeyModifiers::NONE);
        let mut app = note_open();
        note_feed(&mut app, &note_char('x'));
        note_feed(&mut app, &backspace);
        assert_eq!(app.note.as_ref().expect("open").text(), "");
        assert_eq!(
            note_feed(&mut app, &backspace),
            (Changed::No, None),
            "nothing to delete, nothing to draw"
        );
        note_feed(&mut app, &note_char('y'));
        let (changed, effect) =
            note_feed(&mut app, &note_key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            (changed, effect),
            (Changed::Yes, None),
            "Esc writes nothing"
        );
        assert!(app.note.is_none(), "Esc closes the note and writes nothing");
    }

    /// `Shift-Enter` is a newline **only** under the kitty keyboard protocol (ruling P9).
    ///
    /// With no enhancement flags the terminal sends the same bytes for `Enter` and
    /// `Shift-Enter`, so treating the reported `SHIFT` as a line break would mean the note
    /// sometimes breaks and sometimes sends depending on which terminal happens to set the
    /// bit — the worst of the two. Off, it sends; on, it breaks the line.
    #[test]
    fn app_note_modal_shift_enter_is_a_newline_only_with_enhancement() {
        let shift_enter = note_key_event(KeyCode::Enter, KeyModifiers::SHIFT);

        let mut plain = note_open();
        assert!(!plain.enhanced, "the default is the honest one");
        type_note(&mut plain, "one");
        let (_, effect) = note_feed(&mut plain, &shift_enter);
        assert!(plain.note.is_none(), "without enhancement it sends");
        let Some(Effect::Flag { note, .. }) = effect else {
            panic!("a flag effect: {effect:?}");
        };
        assert_eq!(note, "one");

        let mut enhanced = note_open();
        enhanced.enhanced = true;
        type_note(&mut enhanced, "one");
        assert_eq!(note_feed(&mut enhanced, &shift_enter), (Changed::Yes, None));
        type_note(&mut enhanced, "2");
        assert_eq!(
            enhanced.note.as_ref().expect("open").text(),
            "one\n2",
            "with enhancement it breaks the line"
        );

        // The key line promises exactly what the mapping does.
        assert!(!super::super::render::note_keys(false).contains('⇧'));
        assert!(super::super::render::note_keys(true).contains("⇧⏎ / ^J newline"));
    }

    /// Every motion the shared buffer knows reaches the note: arrows, word jumps,
    /// `Ctrl-A`/`Ctrl-E`, `Ctrl-K`, `Ctrl-W`, and the page keys.
    #[test]
    fn app_note_modal_arrow_and_word_keys_move_the_caret() {
        let mut app = note_open();
        type_note(&mut app, "alpha beta gamma");
        let pos = |app: &App| app.note.as_ref().expect("open").buf.cursor;
        assert_eq!(pos(&app), Pos { line: 0, col: 16 });

        // Left, then a word jump back over `gamma`, then Home and End.
        assert_eq!(
            note_feed(&mut app, &note_key_event(KeyCode::Left, KeyModifiers::NONE)),
            (Changed::Yes, None)
        );
        assert_eq!(pos(&app), Pos { line: 0, col: 15 });
        note_feed(&mut app, &note_key_event(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(pos(&app).col, 11, "the start of `gamma`");
        note_feed(
            &mut app,
            &note_key_event(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        assert_eq!(pos(&app).col, 0, "Ctrl-A is the start of the line");
        note_feed(
            &mut app,
            &note_key_event(KeyCode::Right, KeyModifiers::CONTROL),
        );
        assert_eq!(pos(&app).col, 5, "Ctrl-→ is the end of `alpha`");
        note_feed(
            &mut app,
            &note_key_event(KeyCode::Char('e'), KeyModifiers::CONTROL),
        );
        assert_eq!(pos(&app).col, 16, "Ctrl-E is the end of the line");

        // Ctrl-W eats the word behind the caret; Ctrl-K the rest of the line.
        note_feed(
            &mut app,
            &note_key_event(KeyCode::Char('w'), KeyModifiers::CONTROL),
        );
        assert_eq!(app.note.as_ref().expect("open").text(), "alpha beta ");
        note_feed(
            &mut app,
            &note_key_event(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        note_feed(
            &mut app,
            &note_key_event(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        assert_eq!(app.note.as_ref().expect("open").text(), "");

        // A move with nowhere to go is not a frame.
        assert_eq!(
            note_feed(&mut app, &note_key_event(KeyCode::Up, KeyModifiers::NONE)),
            (Changed::No, None)
        );
        assert_eq!(
            note_feed(
                &mut app,
                &note_key_event(KeyCode::PageDown, KeyModifiers::NONE)
            ),
            (Changed::No, None)
        );
    }

    /// A bracketed paste is one event carrying many characters, newlines included. It is
    /// inserted whole and never taken as the Enter that sends — the guard that stops a
    /// pasted multi-line note from firing off its first line.
    #[test]
    fn app_note_modal_paste_event_inserts_and_never_sends() {
        let km = Keymap::defaults();
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Flag);

        let pasted = "first line\nsecond line\n";
        let action = note_action(&Event::Paste(pasted.to_owned()), &km, app.enhanced)
            .expect("paste is handled");
        assert_eq!(
            action,
            Action::Note(NoteKey::Edit(EditKey::Insert(pasted.to_owned())))
        );
        let (changed, effect) = app.handle(action);
        assert_eq!(
            (changed, effect),
            (Changed::Yes, None),
            "inserted, not sent"
        );
        let note = app.note.as_ref().expect("still open");
        assert_eq!(note.text(), pasted);
        assert_eq!(
            note.buf.cursor,
            Pos { line: 2, col: 0 },
            "the caret is after the paste, on the line its last newline opened"
        );

        // A second paste lands after the first, and Enter is still what sends.
        app.handle(note_action(&Event::Paste("third".to_owned()), &km, false).expect("handled"));
        let (_, effect) = app.handle(Action::Note(NoteKey::Send));
        let Some(Effect::Flag { note, .. }) = effect else {
            panic!("a flag effect: {effect:?}");
        };
        assert_eq!(note, "first line\nsecond line\nthird");
    }

    /// While the note is open the keymap is off: `q` and `a` are text, not quit and accept.
    /// `Ctrl-C` is the exception — a modal must never be a trap you cannot leave — and it
    /// quits without writing the note.
    #[test]
    fn app_note_modal_swallows_keymap_keys_but_ctrl_c_quits() {
        let km = Keymap::defaults();
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Flag);

        for c in "qa?rewfo".chars() {
            assert_eq!(
                note_action(&note_char(c), &km, false),
                Some(Action::Note(NoteKey::Edit(EditKey::Insert(c.to_string())))),
                "{c} is text inside the modal"
            );
            app.handle(Action::Note(NoteKey::Edit(EditKey::Insert(c.to_string()))));
        }
        assert_eq!(app.note.as_ref().expect("open").text(), "qa?rewfo");
        assert!(!app.help, "no key escaped to the keymap");
        assert!(app.confirm.is_none());

        // A shifted letter is still a letter: `A` types an `A`, it does not accept the file.
        assert_eq!(
            note_action(
                &note_key_event(KeyCode::Char('A'), KeyModifiers::SHIFT),
                &km,
                false
            ),
            Some(Action::Note(NoteKey::Edit(EditKey::Insert("A".to_owned()))))
        );

        // Keys the buffer has no answer for are swallowed rather than reaching the keymap.
        for event in [
            note_key_event(KeyCode::F(1), KeyModifiers::NONE),
            note_key_event(KeyCode::Insert, KeyModifiers::NONE),
            note_key_event(KeyCode::Char('x'), KeyModifiers::CONTROL),
        ] {
            assert_eq!(
                note_action(&event, &km, false),
                None,
                "{event:?} does nothing"
            );
        }

        // Ctrl-C is the one binding that still fires, and it does not write the note.
        let ctrl_c = note_key_event(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(note_action(&ctrl_c, &km, false), Some(Action::Quit));
        let (_, effect) = app.handle(Action::Quit);
        assert_eq!(effect, Some(Effect::Quit), "quitting writes no flag");
    }

    // ---- deliverable 11: the reducer's remaining promises -------------------------------

    /// From the nav there is no diff cursor to read, so both editor keys open at the row's
    /// **first** hunk — and a row with no hunks at all opens at line 1.
    #[test]
    fn app_edit_external_from_the_nav_uses_the_first_hunk_line() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        // Three hunks at known places; `f1`'s own first line is a deletion, so each hunk's
        // editor line is its `new_range.start + 1`.
        app.apply(pile_event_seq(
            "alpha",
            1,
            alpha_ranges(&[(4, 8), (20, 24), (40, 44)]),
        ));
        app.select(Some(row("alpha", "f1")));
        assert_eq!(app.effective_focus(), Focus::Nav);
        let (_, effect) = app.handle(Action::EditExternal);
        let Some(Effect::EditExternal { line, .. }) = effect else {
            panic!("an external-edit effect, got {effect:?}");
        };
        assert_eq!(line, 5, "the first hunk's first changed line");

        // In the diff the cursor decides, which is the whole reason the nav needs a rule.
        app.handle(Action::Open);
        app.handle(Action::HunkNext);
        let (_, effect) = app.handle(Action::EditExternal);
        let Some(Effect::EditExternal { line, .. }) = effect else {
            panic!("an external-edit effect, got {effect:?}");
        };
        assert_eq!(line, 21, "the hunk under the cursor");

        // A collapsed row carries no hunks: there is no line to name, so it opens at 1.
        let mut collapsed = three_roots();
        collapsed.handle(Action::Resize(100, 30));
        collapsed.apply(pile_event_seq("alpha", 1, alpha_collapsed(Collapsed::Glob)));
        collapsed.select(Some(row("alpha", "f1")));
        let (_, effect) = collapsed.handle(Action::EditExternal);
        let Some(Effect::EditExternal { line, .. }) = effect else {
            panic!("an external-edit effect, got {effect:?}");
        };
        assert_eq!(line, 1);
    }

    /// Every modal owns the keyboard while it is open, and that includes the three keys
    /// Phase 8 added: `i`, `shift-i` and `v` do nothing under the note, the picker or the
    /// confirm, and nothing under the help overlay but close it.
    #[test]
    fn app_edit_and_select_are_swallowed_while_a_modal_is_open() {
        let keys = [
            Action::Edit,
            Action::EditExternal,
            Action::Select,
            Action::Copy,
        ];

        // The confirm: a restore is waiting on an answer, and an editor opened over it
        // would take the keys that answer it.
        let mut confirm = diff_at_f1();
        confirm.handle(Action::RestoreFile);
        assert!(confirm.confirm.is_some());
        for action in keys.clone() {
            assert_eq!(
                confirm.handle(action.clone()),
                (Changed::No, None),
                "{action:?}"
            );
        }
        assert!(confirm.confirm.is_some(), "and the question is still up");
        assert!(confirm.editor.is_none() && confirm.sel.is_none());

        // The note modal, which is itself a text field: `i` and `v` are characters in it,
        // and `Ui::event` never turns them into these actions — but the reducer refuses
        // them too, so neither path can open an editor under a modal.
        let mut note = diff_at_f1();
        note.handle(Action::Flag);
        assert!(note.note.is_some());
        for action in keys.clone() {
            assert_eq!(
                note.handle(action.clone()),
                (Changed::No, None),
                "{action:?}"
            );
        }
        assert!(note.note.is_some() && note.editor.is_none() && note.sel.is_none());

        // The help overlay is not a modal but a layer: the first key closes it and is
        // spent doing so, exactly as every other key is.
        for action in keys {
            let mut help = diff_at_f1();
            help.help = true;
            assert_eq!(
                help.handle(action.clone()),
                (Changed::Yes, None),
                "{action:?}"
            );
            assert!(!help.help, "{action:?} closed the overlay");
            assert!(help.editor.is_none() && help.sel.is_none(), "{action:?}");
        }
    }

    // ---- deliverable 9: select to copy ---------------------------------------------------

    /// alpha with `f1` given two identical hunks, focused in the diff at line 0.
    ///
    /// Each hunk is the fixture's own: header `@@ -1,4 +1,4 @@` then `-a1`, `+A1`, ` a2`,
    /// ` a3`, ` a4` — six diff lines, a blank separator, six more.
    fn diff_at_f1() -> App {
        let mut app = three_roots();
        app.apply(pile_event("alpha", alpha_two_hunks()));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);
        assert_eq!(app.effective_focus(), Focus::Diff);
        assert_eq!(diff_lines(app.view_hunks()), 13);
        app
    }

    fn copied(effect: Option<Effect>) -> String {
        match effect {
            Some(Effect::Copy(bytes)) => String::from_utf8(bytes).expect("utf-8 payload"),
            other => panic!("expected a copy, got {other:?}"),
        }
    }

    #[test]
    fn app_select_extends_with_the_cursor_and_y_copies_the_range() {
        let mut app = diff_at_f1();
        // There is no per-line cursor in the diff pane: the cursor *is* the first visible
        // line, so `v j j y` copies three lines counted from where the reader was.
        assert_eq!(app.diff.scroll, 0);
        assert_eq!(app.handle(Action::Select), (Changed::Yes, None));
        assert_eq!(
            app.sel,
            Some(Sel {
                anchor: 0,
                cursor: 0
            })
        );
        for _ in 0..2 {
            app.handle(Action::NavDown);
        }
        // The pane does *not* scroll under the selection: the far end moved, and the three
        // selected lines are the three the reader can see.
        assert_eq!(app.diff.scroll, 0);
        assert_eq!(
            app.sel,
            Some(Sel {
                anchor: 0,
                cursor: 2
            })
        );
        let (changed, effect) = app.handle(Action::Copy);
        assert_eq!(changed, Changed::Yes);
        assert_eq!(copied(effect), "@@ -1,4 +1,4 @@\n-a1\n+A1\n");
        assert_eq!(app.sel, None, "a copy clears the selection");
        assert_eq!(app.cue.as_ref().map(|c| c.text.as_str()), Some(COPIED));

        // Upwards from the middle: the range is whichever way round the two ends are, and
        // the pane scrolls only as far as it takes to keep the far end on screen.
        for _ in 0..3 {
            app.handle(Action::NavDown);
        }
        assert_eq!(
            app.diff.scroll, 3,
            "with no selection the keys still scroll"
        );
        app.handle(Action::Select);
        app.handle(Action::NavUp);
        app.handle(Action::NavUp);
        assert_eq!(
            app.sel,
            Some(Sel {
                anchor: 3,
                cursor: 1
            })
        );
        assert_eq!(app.diff.scroll, 1, "the far end pulled the pane up with it");
        assert_eq!(copied(app.handle(Action::Copy).1), "-a1\n+A1\n a2\n");

        // Esc clears without copying, and leaves the pane where it was.
        app.handle(Action::Select);
        assert_eq!(app.handle(Action::Back), (Changed::Yes, None));
        assert_eq!(app.sel, None);
        assert_eq!(app.effective_focus(), Focus::Diff, "Esc peeled one layer");
        assert_eq!(app.handle(Action::Back), (Changed::Yes, None));
        assert_eq!(app.effective_focus(), Focus::Nav, "the second one focuses");
        // And with the nav focused neither key does anything.
        assert_eq!(app.handle(Action::Select), (Changed::No, None));
        assert_eq!(app.handle(Action::Copy), (Changed::No, None));
    }

    #[test]
    fn app_y_without_a_selection_copies_the_hunk() {
        let mut app = diff_at_f1();
        let whole = "@@ -1,4 +1,4 @@\n-a1\n+A1\n a2\n a3\n a4\n";
        assert_eq!(copied(app.handle(Action::Copy).1), whole);
        assert_eq!(app.sel, None);
        // The *hunk under the cursor*, not the first: `n` moves the cursor and `y` follows.
        app.handle(Action::HunkNext);
        assert_eq!(app.diff.hunk, 1);
        assert_eq!(copied(app.handle(Action::Copy).1), whole);
        // A selection that spans the separator carries it, exactly as the pane shows it.
        app.handle(Action::HunkPrev);
        app.handle(Action::Select);
        for _ in 0..8 {
            app.handle(Action::NavDown);
        }
        assert_eq!(
            copied(app.handle(Action::Copy).1),
            "@@ -1,4 +1,4 @@\n-a1\n+A1\n a2\n a3\n a4\n\n@@ -1,4 +1,4 @@\n-a1\n"
        );
        // Nothing selected and nothing to select: a row with no hunks copies nothing.
        let mut empty = three_roots();
        empty.select(Some(row("alpha", "f1")));
        empty.apply(pile_event("alpha", alpha_collapsed(Collapsed::Glob)));
        empty.handle(Action::Open);
        assert_eq!(empty.handle(Action::Copy), (Changed::No, None));
    }

    #[test]
    fn app_copy_cue_lasts_two_seconds_and_leaves_the_status_alone() {
        let mut app = diff_at_f1();
        app.set_status("saved f1");
        app.handle(Action::Copy);
        assert_eq!(app.cue.as_ref().map(|c| c.text.as_str()), Some(COPIED));
        assert_eq!(
            app.status.as_ref().map(|s| s.text.as_str()),
            Some("saved f1"),
            "a copy is a thing the terminal did; the status is the engine's record"
        );
        // One second in, the cue is still up; the second tick reaches `until` and clears it.
        assert_eq!(app.handle(Action::Tick).0, Changed::Yes);
        assert!(app.cue.is_some(), "one second is not two");
        assert_eq!(app.handle(Action::Tick).0, Changed::Yes);
        assert_eq!(app.cue, None);
        assert_eq!(
            app.status.as_ref().map(|s| s.text.as_str()),
            Some("saved f1"),
            "and the status outlives it"
        );
    }

    #[test]
    fn app_mouse_drag_selects_rows_and_release_copies() {
        let mut app = diff_at_f1();
        // A press with no drag after it is a click, not a zero-length copy — the loop
        // clears `sel` on the press and nothing here puts one back.
        app.press_line = Some(1);
        assert_eq!(app.handle(Action::Release), (Changed::No, None));
        assert_eq!(app.sel, None);
        assert_eq!(app.press_line, None, "the release ends the gesture");

        // A drag from line 1 to line 4, then the release that copies it.
        app.press_line = Some(1);
        assert_eq!(app.handle(Action::SelectTo(2)).0, Changed::Yes);
        assert!(app.drag_moved);
        assert_eq!(app.handle(Action::SelectTo(4)).0, Changed::Yes);
        assert_eq!(
            app.sel,
            Some(Sel {
                anchor: 1,
                cursor: 4
            })
        );
        assert_eq!(app.handle(Action::SelectTo(4)).0, Changed::No, "no repaint");
        let (changed, effect) = app.handle(Action::Release);
        assert_eq!(changed, Changed::Yes);
        assert_eq!(copied(effect), "-a1\n+A1\n a2\n a3\n");
        assert_eq!(app.sel, None);
        assert!(!app.drag_moved);

        // A drag whose press did not land in the diff body — the divider's, or the nav's —
        // selects nothing at all (design review F13).
        assert_eq!(app.press_line, None);
        assert_eq!(app.handle(Action::SelectTo(3)), (Changed::No, None));
        assert_eq!(app.sel, None);
        // And a drag past the last diff line stops at it rather than copying blanks.
        app.press_line = Some(11);
        app.handle(Action::SelectTo(999));
        assert_eq!(
            app.sel,
            Some(Sel {
                anchor: 11,
                cursor: 12
            })
        );
    }

    #[test]
    fn app_copy_over_the_cap_writes_nothing() {
        let mut app = three_roots();
        let mut big = pile("alpha");
        big.rows[0].hunks[0].lines = vec![(Tag::Insert, vec![b'x'; 40 * 1024])];
        app.apply(pile_event("alpha", big));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);
        let payload = app.copy_payload().expect("a payload to refuse");
        assert!(payload.len() > super::super::clipboard::CAP);
        let (changed, effect) = app.handle(Action::Copy);
        assert_eq!(changed, Changed::Yes);
        assert_eq!(effect, None, "nothing is written over the cap");
        assert_eq!(app.cue, None, "and no cue claims otherwise");
        assert_eq!(
            app.status.as_ref().map(|s| s.text.as_str()),
            Some("selection too large to copy (41 KiB; the terminal would drop it)")
        );
        // The selection stays: the only thing the reader can do is select less.
        app.handle(Action::Select);
        app.handle(Action::NavDown);
        let sel = app.sel;
        app.handle(Action::Copy);
        assert_eq!(app.sel, sel, "still there to shrink");
    }

    // ---- deliverable 2: undo ---------------------------------------------------------------

    /// `z` asks the engine for this root's last entry and, when it comes back, puts the
    /// cursor on the file that came with it — the whole point of the gesture is that the
    /// row you accepted by mistake is under the cursor again.
    #[test]
    fn app_z_undoes_the_selected_root_and_puts_the_cursor_on_the_file() {
        let mut app = three_roots();
        // alpha with `f1` accepted and one entry on the stack.
        app.apply(pile_event_seq(
            "alpha",
            1,
            undo_pile(without(pile("alpha"), &["f1"]), 1),
        ));
        app.select(Some(row("alpha", "f2")));
        let (changed, effect) = app.handle(Action::Undo);
        assert_eq!(changed, Changed::Yes);
        assert_eq!(effect, Some(Effect::Undo(root("alpha"))));
        assert_eq!(status(&app), "undoing…");
        assert_eq!(app.undoing.as_deref(), Some(root("alpha").as_path()));

        let (r, res) = undone_ok("alpha", 2, &["f1"], undo_pile(pile("alpha"), 0));
        assert_eq!(app.undone(r, res), Changed::Yes);
        assert_eq!(status(&app), "undid accept of f1");
        assert_eq!(app.selection, Some(row("alpha", "f1")));
        assert_eq!(app.undoing, None, "the key works again");
    }

    /// A stack the pile already reports as empty is answered by the reducer: no effect, so
    /// no engine work and no ledger lock for an answer that is already known.
    #[test]
    fn app_z_on_an_empty_stack_says_so_without_asking_the_engine() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        let (changed, effect) = app.handle(Action::Undo);
        assert_eq!(changed, Changed::Yes);
        assert_eq!(effect, None, "the pile already said `undo: 0`");
        assert_eq!(status(&app), "nothing to undo in alpha");
        assert_eq!(app.undoing, None);
        // And a second `z` while one is in flight is refused rather than queued.
        app.apply(pile_event_seq("alpha", 1, undo_pile(pile("alpha"), 1)));
        assert!(app.handle(Action::Undo).1.is_some());
        assert_eq!(app.handle(Action::Undo), (Changed::Yes, None));
        assert_eq!(status(&app), "undo in progress");
    }

    /// `ctrl-a` writes one entry per root, so undoing in one of them leaves the others
    /// accepted. The sentence says how many, rather than letting `z` read as a whole-sweep
    /// undo (§6.7 as amended by v1.11).
    #[test]
    fn app_z_after_an_accept_all_names_the_other_repos() {
        let mut app = three_roots();
        app.select(Some(Selection::Root(root("alpha"))));
        app.handle(Action::AcceptAll);
        app.accepted(vec![
            accepted_ok("alpha", 2, undo_pile(Pile::default(), 1)),
            accepted_ok("beta", 2, undo_pile(Pile::default(), 1)),
            accepted_ok("notes", 2, undo_pile(Pile::default(), 1)),
        ]);
        assert_eq!(app.accept_all_roots.len(), 3, "all three were swept");

        app.select(Some(Selection::Root(root("alpha"))));
        assert_eq!(
            app.handle(Action::Undo).1,
            Some(Effect::Undo(root("alpha")))
        );
        let (r, res) = undone_ok("alpha", 3, &["f1", "f2"], undo_pile(pile("alpha"), 0));
        app.undone(r, res);
        assert_eq!(
            status(&app),
            "undid accept of 2 files in alpha (2 other repos have their own undo)"
        );

        // Once the others' stacks are gone the clause goes with them.
        app.apply(pile_event_seq("beta", 3, undo_pile(Pile::default(), 0)));
        app.apply(pile_event_seq("notes", 3, undo_pile(Pile::default(), 0)));
        app.select(Some(Selection::Root(root("alpha"))));
        app.apply(pile_event_seq("alpha", 4, undo_pile(pile("alpha"), 1)));
        app.handle(Action::Undo);
        let (r, res) = undone_ok("alpha", 5, &["f1"], undo_pile(pile("alpha"), 0));
        app.undone(r, res);
        assert_eq!(status(&app), "undid accept of f1");
    }

    /// An engine that found nothing (a second process drained the stack between the pile
    /// and the key) says so in the same words the reducer's own short circuit uses.
    #[test]
    fn app_z_against_a_stack_another_process_drained_says_nothing_to_undo() {
        let mut app = three_roots();
        app.apply(pile_event_seq("beta", 1, undo_pile(pile("beta"), 1)));
        app.select(Some(Selection::Root(root("beta"))));
        app.handle(Action::Undo);
        let (r, res) = undone_ok("beta", 2, &[], undo_pile(pile("beta"), 0));
        app.undone(r, res);
        assert_eq!(status(&app), "nothing to undo in beta");
        assert_eq!(app.selection, Some(Selection::Root(root("beta"))));
    }

    // ---- deliverable 3: snooze -------------------------------------------------------------

    /// `s` on a file row would have to guess which repository the reader meant. It says so
    /// instead, and opens nothing.
    #[test]
    fn app_s_needs_a_repository_row() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        let (changed, effect) = app.handle(Action::Snooze);
        assert_eq!(changed, Changed::Yes);
        assert_eq!(effect, None);
        assert_eq!(status(&app), SNOOZE_NEEDS_ROOT);
        assert_eq!(app.snooze, None, "no modal opened");
    }

    /// The modal's field is the digits as typed: it seeds with the default, takes digits,
    /// refuses one that would leave the 1..=365 range, backspaces to empty, and an empty
    /// field applies the default rather than zero.
    #[test]
    fn app_snooze_modal_edits_the_day_count_and_clamps_the_range() {
        let mut app = three_roots();
        app.select(Some(Selection::Root(root("alpha"))));
        assert_eq!(app.handle(Action::Snooze), (Changed::Yes, None));
        let entry = app.snooze.clone().expect("the modal is open");
        assert_eq!(entry.days, SNOOZE_DEFAULT_DAYS.to_string());
        assert_eq!(entry.name, "alpha");

        let digit = |app: &mut App, c: char| app.handle(Action::SnoozeEdit(SnoozeKey::Digit(c)));
        // `1` then `4` is 14 days.
        assert_eq!(digit(&mut app, '4'), (Changed::Yes, None));
        assert_eq!(app.snooze.as_ref().unwrap().days, "14");
        assert_eq!(app.snooze.as_ref().unwrap().value(), 14);
        // 145 still fits; 1450 does not, and the field is left showing 145.
        assert_eq!(digit(&mut app, '5'), (Changed::Yes, None));
        assert_eq!(digit(&mut app, '0'), (Changed::No, None));
        assert_eq!(app.snooze.as_ref().unwrap().days, "145");

        // Backspace to empty; the field shows nothing rather than jumping to 0.
        for _ in 0..3 {
            assert_eq!(
                app.handle(Action::SnoozeEdit(SnoozeKey::Backspace)),
                (Changed::Yes, None)
            );
        }
        assert_eq!(app.snooze.as_ref().unwrap().days, "");
        assert_eq!(
            app.handle(Action::SnoozeEdit(SnoozeKey::Backspace)),
            (Changed::No, None),
            "nothing left to delete, nothing to redraw"
        );
        assert_eq!(app.snooze.as_ref().unwrap().value(), SNOOZE_DEFAULT_DAYS);
        // A leading zero is not a number anyone typed on purpose.
        digit(&mut app, '0');
        assert_eq!(app.snooze.as_ref().unwrap().days, "");
        digit(&mut app, '7');
        assert_eq!(app.snooze.as_ref().unwrap().days, "7");

        // Esc closes it and writes nothing.
        assert_eq!(
            app.handle(Action::SnoozeEdit(SnoozeKey::Cancel)),
            (Changed::Yes, None)
        );
        assert_eq!(app.snooze, None);
        assert_eq!(app.snoozing, None);

        // Enter applies the number the field is showing.
        app.handle(Action::Snooze);
        digit(&mut app, '3');
        let (changed, effect) = app.handle(Action::SnoozeEdit(SnoozeKey::Apply));
        assert_eq!(changed, Changed::Yes);
        assert_eq!(
            effect,
            Some(Effect::Snooze {
                root: root("alpha"),
                days: Some(13),
            })
        );
        assert_eq!(app.snooze, None, "the modal closed with the key");
    }

    /// The deadline the engine wrote takes the repository off the nav; `shift-s` shows it
    /// again, and `s` on a repository that is already snoozed wakes it without a modal.
    #[test]
    fn app_snooze_takes_the_repo_off_the_nav_and_shift_s_shows_it() {
        let mut app = three_roots();
        app.select(Some(Selection::Root(root("beta"))));
        app.handle(Action::Snooze);
        app.handle(Action::SnoozeEdit(SnoozeKey::Apply));
        let (r, res) = snoozed_ok(
            "beta",
            2,
            Some("2026-09-20T09:00:00Z"),
            snoozed_pile(pile("beta"), "2026-09-20T09:00:00Z"),
        );
        assert_eq!(app.snoozed_result(r, res), Changed::Yes);
        assert_eq!(status(&app), "snoozed beta until 2026-09-20");
        assert_eq!(app.snoozing, None);

        let listed = |app: &App| -> Vec<String> {
            app.roots
                .values()
                .filter(|v| app.is_listed(v))
                .map(|v| v.meta.name.clone())
                .collect()
        };
        assert_eq!(listed(&app), vec!["alpha".to_owned(), "notes".to_owned()]);
        assert_eq!(app.snoozed_out(), 1);
        assert_eq!(
            app.snooze_notice().as_deref(),
            Some("1 snoozed (S shows)"),
            "the keymap canonicalises shift-s to S"
        );

        // `shift-s` shows it again, and the notice goes with it.
        assert_eq!(app.handle(Action::ShowSnoozed).0, Changed::Yes);
        assert_eq!(
            listed(&app),
            vec!["alpha".to_owned(), "beta".to_owned(), "notes".to_owned()]
        );
        assert_eq!(app.snoozed_out(), 0);
        assert_eq!(app.snooze_notice(), None);
        assert_eq!(
            app.snoozed(&app.roots[&root("beta")]),
            Some("2026-09-20T09:00:00Z"),
            "shown is not the same as awake"
        );

        // `s` on it now wakes it: no modal, no question.
        app.select(Some(Selection::Root(root("beta"))));
        let (changed, effect) = app.handle(Action::Snooze);
        assert_eq!(changed, Changed::Yes);
        assert_eq!(app.snooze, None, "nothing to ask");
        assert_eq!(
            effect,
            Some(Effect::Snooze {
                root: root("beta"),
                days: None,
            })
        );
        let (r, res) = snoozed_ok("beta", 3, None, pile("beta"));
        app.snoozed_result(r, res);
        assert_eq!(status(&app), "woke beta");
        assert_eq!(app.snoozed(&app.roots[&root("beta")]), None);
    }

    /// A snoozed repository whose agent wants attention is listed anyway — the `hide_empty`
    /// exception exactly, and for the same reason (Amendment v1.11).
    #[test]
    fn app_a_snoozed_repo_with_attention_is_listed_anyway() {
        use crate::tui::herdr::{Attention, HerdrUpdate, RootAgents};
        let mut app = three_roots();
        app.apply(pile_event_seq(
            "beta",
            1,
            snoozed_pile(pile("beta"), "2026-09-20T09:00:00Z"),
        ));
        assert!(!app.is_listed(&app.roots[&root("beta")]));
        assert_eq!(app.snoozed_out(), 1);

        app.handle(Action::Herdr(HerdrUpdate::Connected {
            version: "0.8.2".to_owned(),
            protocol: 21,
        }));
        app.handle(Action::Herdr(HerdrUpdate::Roots(
            [(
                root("beta"),
                RootAgents {
                    status: Attention::Blocked,
                    agents: 1,
                    pane: Some("w1:p1".to_owned()),
                    agent: Some("claude".to_owned()),
                },
            )]
            .into_iter()
            .collect(),
        )));
        assert!(
            app.is_listed(&app.roots[&root("beta")]),
            "a blocked agent is news the reader asked for"
        );
        assert_eq!(
            app.snoozed_out(),
            0,
            "and the notice does not promise a row that is already on the nav"
        );
    }

    /// Design review F4: the TUI has no wall clock, so the loop hands `Tick` the engine's.
    /// A deadline the clock has passed is dropped and the repository comes back, without
    /// waiting for a scan or a restart.
    #[test]
    fn app_a_snooze_that_expires_under_the_cursor_comes_back_on_the_next_tick() {
        let mut app = three_roots();
        let deadline = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_789_344_000);
        let until = lastcall_engine::ledger::iso8601(deadline);
        assert_eq!(until, "2026-09-14T00:00:00Z");
        app.apply(pile_event_seq(
            "beta",
            1,
            snoozed_pile(pile("beta"), &until),
        ));
        assert!(!app.is_listed(&app.roots[&root("beta")]));

        // A tick a second short of the deadline leaves it alone.
        app.wall = Some(deadline - std::time::Duration::from_secs(1));
        app.handle(Action::Tick);
        assert!(!app.is_listed(&app.roots[&root("beta")]));

        // The deadline itself is the boundary: at it, the snooze is over.
        app.wall = Some(deadline);
        assert_eq!(app.handle(Action::Tick).0, Changed::Yes);
        assert_eq!(app.roots[&root("beta")].pile.snoozed_until, None);
        assert!(app.is_listed(&app.roots[&root("beta")]));
        assert_eq!(app.snoozed_out(), 0);
    }

    /// A draft root has no git of its own, but the ledger and the nav treat it like any
    /// other root, so `s` works on it (§5 draft roots, Amendment v1.11).
    #[test]
    fn app_a_draft_root_snoozes_like_a_git_root() {
        let mut app = three_roots();
        assert_eq!(app.roots[&root("notes")].meta.kind, RootKind::Draft);
        app.select(Some(Selection::Root(root("notes"))));
        app.handle(Action::Snooze);
        assert_eq!(app.snooze.as_ref().map(|e| e.name.as_str()), Some("notes"));
        let (_, effect) = app.handle(Action::SnoozeEdit(SnoozeKey::Apply));
        assert_eq!(
            effect,
            Some(Effect::Snooze {
                root: root("notes"),
                days: Some(SNOOZE_DEFAULT_DAYS),
            })
        );
        let (r, res) = snoozed_ok(
            "notes",
            2,
            Some("2026-09-15T00:00:00Z"),
            snoozed_pile(pile("notes"), "2026-09-15T00:00:00Z"),
        );
        app.snoozed_result(r, res);
        assert_eq!(status(&app), "snoozed notes until 2026-09-15");
        assert!(!app.is_listed(&app.roots[&root("notes")]));
    }

    /// Design review F6: the bottom-line notice shrinks a form at a time, and only when a
    /// scope count and a snooze count are on the line together. A scope count on its own
    /// keeps the mandatory wording it has had since Phase 9.
    #[test]
    fn app_bottom_notice_shrinks_only_when_both_counts_are_on_the_line() {
        use crate::tui::herdr::{HerdrUpdate, Scope};
        let mut app = three_roots();
        app.herdr.scoped = true;
        app.handle(Action::Herdr(HerdrUpdate::Scope(Some(Scope {
            label: "alpha".to_owned(),
            roots: [root("alpha"), root("beta")].into_iter().collect(),
        }))));

        // Scope alone: one form, at every width.
        assert_eq!(
            app.notice_forms(),
            vec!["scope: alpha · 1 repo hidden (w shows all)".to_owned()]
        );
        assert_eq!(
            app.bottom_notice(40).as_deref(),
            Some("scope: alpha · 1 repo hidden (w shows all)"),
            "a narrow frame gives it the row rather than shortening it"
        );

        // With a snooze as well there are three, longest first.
        app.apply(pile_event_seq(
            "beta",
            1,
            snoozed_pile(pile("beta"), "2026-09-20T09:00:00Z"),
        ));
        assert_eq!(
            app.notice_forms(),
            vec![
                "scope: alpha · 1 repo hidden (w shows all) · 1 snoozed (S shows)".to_owned(),
                "scope: alpha · 1 hidden · 1 snoozed".to_owned(),
                "1 hidden · 1 snoozed".to_owned(),
            ]
        );
        assert_eq!(
            app.bottom_notice(100).as_deref(),
            Some("scope: alpha · 1 repo hidden (w shows all) · 1 snoozed (S shows)")
        );
        assert_eq!(
            app.bottom_notice(80).as_deref(),
            Some("scope: alpha · 1 hidden · 1 snoozed")
        );
        assert_eq!(
            app.bottom_notice(50).as_deref(),
            Some("1 hidden · 1 snoozed")
        );
        assert_eq!(
            app.bottom_notice(20).as_deref(),
            Some("1 hidden · 1 snoozed"),
            "below the shortest form it stands and takes the row"
        );

        // Nothing to report at all: no notice, and the hints keep the row.
        let mut plain = three_roots();
        assert_eq!(plain.notice_forms(), Vec::<String>::new());
        assert_eq!(plain.bottom_notice(100), None);
        plain.apply(pile_event_seq(
            "beta",
            1,
            snoozed_pile(pile("beta"), "2026-09-20T09:00:00Z"),
        ));
        assert_eq!(
            plain.notice_forms(),
            vec!["1 snoozed (S shows)".to_owned()],
            "a snooze count on its own has one form too"
        );
    }
    // ---- the first-launch welcome (Amendment v1.11, deliverable 1) --------------------

    use crate::tui::tour::{Card, Tour};

    /// A three-root app with the tour open on `cards`.
    fn with_tour(cards: Vec<Card>) -> App {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.tour = Some(Tour::new(cards));
        app
    }

    fn empty_card() -> Card {
        Card::Empty {
            empty: 12,
            total: 14,
        }
    }

    /// The depth card: second in the tour, a choice card, and the only one whose condition
    /// is about the config file alone, so it is the filler for "the next card" here.
    fn depth_card() -> Card {
        Card::Depth {
            path: Some("/c/config.toml".to_owned()),
        }
    }

    /// Whether `name` is on screen, through the same gate the painter uses.
    fn listed(app: &App, name: &str) -> bool {
        app.listed_roots().any(|v| v.meta.path == root(name))
    }

    fn herdr_card() -> Card {
        Card::Herdr {
            version: "0.8.2".to_owned(),
        }
    }

    /// F7: the overlay sits above everything. While it is up the keymap's actions do not
    /// reach the screen underneath — `?` does not open the help overlay behind it, `a`
    /// accepts nothing, `t` hides nothing — and the frame does not even redraw.
    #[test]
    fn app_tour_holds_every_key_that_is_not_its_own() {
        let mut app = with_tour(vec![Card::Keys]);
        app.select(Some(row("alpha", "f1")));
        let before = app.selection.clone();
        for action in [
            Action::Help,
            Action::Accept,
            Action::AcceptFile,
            Action::AcceptAll,
            Action::HideEmpty,
            Action::NavDown,
            Action::Open,
            Action::Snooze,
            Action::Undo,
            Action::Refresh,
        ] {
            assert_eq!(
                app.handle(action.clone()),
                (Changed::No, None),
                "{action:?} reached the screen under the welcome"
            );
        }
        assert!(!app.help, "no help overlay opened behind it");
        assert!(!app.hide_empty);
        assert_eq!(app.selection, before, "the cursor did not move");
        assert!(app.tour.is_some(), "and the welcome is still up");
    }

    /// The tour's own keys, a resize, the clock and the news from herdr still land: the
    /// screen behind it stays live and the overlay does not freeze the program.
    #[test]
    fn app_tour_lets_the_frame_stay_live_underneath() {
        let mut app = with_tour(vec![Card::Keys, empty_card()]);
        assert_eq!(app.handle(Action::Resize(80, 24)).0, Changed::Yes);
        assert_eq!(app.size, (80, 24));
        app.handle(Action::Tick);
        assert_eq!(
            app.herdr_update(HerdrUpdate::Connected {
                version: "0.8.2".to_owned(),
                protocol: 1,
            })
            .0,
            Changed::Yes,
            "herdr news still lands"
        );
        assert!(app.tour.is_some());
    }

    /// `enter` on a plain card goes to the next one; `enter` on the last closes the tour
    /// and asks the loop for the marker.
    #[test]
    fn app_tour_enter_walks_the_cards_and_the_last_one_closes_it() {
        let mut app = with_tour(vec![Card::Keys, depth_card()]);
        assert_eq!(
            app.handle(Action::Tour(TourKey::Next)),
            (Changed::Yes, None)
        );
        assert_eq!(app.tour.as_ref().expect("open").at, 1);
        assert_eq!(
            app.handle(Action::Tour(TourKey::Next)),
            (Changed::Yes, Some(Effect::TourDone))
        );
        assert!(app.tour.is_none(), "the last card closes it");
    }

    /// `q` and `esc` skip the rest, from any card, and ask for the marker all the same.
    #[test]
    fn app_tour_skip_closes_it_from_any_card() {
        for at in [0usize, 1] {
            let mut app = with_tour(vec![Card::Keys, empty_card()]);
            app.tour.as_mut().expect("open").at = at;
            assert_eq!(
                app.handle(Action::Tour(TourKey::Skip)),
                (Changed::Yes, Some(Effect::TourDone)),
                "skipped from card {at}"
            );
            assert!(app.tour.is_none());
        }
    }

    /// The arrows move between a choice card's two rows and stop at each end; a plain card
    /// has no rows and says so by not redrawing.
    #[test]
    fn app_tour_arrows_move_between_the_two_choice_rows_only() {
        let mut app = with_tour(vec![Card::Keys]);
        assert_eq!(app.handle(Action::Tour(TourKey::Down)), (Changed::No, None));
        assert_eq!(app.handle(Action::Tour(TourKey::Up)), (Changed::No, None));

        let mut app = with_tour(vec![empty_card()]);
        assert_eq!(app.handle(Action::Tour(TourKey::Up)), (Changed::No, None));
        assert_eq!(
            app.handle(Action::Tour(TourKey::Down)),
            (Changed::Yes, None)
        );
        assert_eq!(app.tour.as_ref().expect("open").row, 1);
        assert_eq!(
            app.handle(Action::Tour(TourKey::Down)),
            (Changed::No, None),
            "the last row is the last row"
        );
        assert_eq!(app.handle(Action::Tour(TourKey::Up)), (Changed::Yes, None));
        assert_eq!(app.tour.as_ref().expect("open").row, 0);
    }

    /// The first row of a choice card is the one already selected, and it writes nothing:
    /// `enter` on it is the same "next card" the keys card's `enter` is.
    #[test]
    fn app_tour_first_choice_row_writes_nothing() {
        let mut app = with_tour(vec![empty_card()]);
        assert_eq!(
            app.handle(Action::Tour(TourKey::Next)),
            (Changed::Yes, Some(Effect::TourDone))
        );
        assert!(!app.hide_empty, "and nothing changed for the session");
    }

    /// The second row applies to this session the instant it is chosen and asks the loop
    /// for the one config write. The card stays up until the loop answers.
    #[test]
    fn app_tour_second_choice_row_applies_now_and_asks_for_the_write() {
        let mut app = with_tour(vec![empty_card()]);
        app.handle(Action::Tour(TourKey::Down));
        assert_eq!(
            app.handle(Action::Tour(TourKey::Next)),
            (
                Changed::Yes,
                Some(Effect::TourWrite(Setting::HideEmptyRepos))
            )
        );
        assert!(app.hide_empty, "applied to this session straight away");
        assert!(app.tour.is_some(), "and the card waits for the answer");
        assert_eq!(
            app.tour_written(Ok(())),
            (Changed::Yes, Some(Effect::TourDone)),
            "the write landed, so the tour moves on"
        );
    }

    /// The herdr card's second row stops following the workspace for the session, the same
    /// thing `w` does, and asks for `scope = "all"`.
    #[test]
    fn app_tour_herdr_choice_stops_following_the_workspace() {
        let mut app = with_tour(vec![herdr_card()]);
        app.herdr.link = Link::Connected {
            version: "0.8.2".to_owned(),
        };
        app.herdr.scope = Some(crate::tui::herdr::Scope {
            label: "W".to_owned(),
            roots: std::collections::BTreeSet::from([root("alpha")]),
        });
        app.herdr.scoped = true;
        assert!(!listed(&app, "beta"), "out of the workspace");
        app.handle(Action::Tour(TourKey::Down));
        assert_eq!(
            app.handle(Action::Tour(TourKey::Next)),
            (
                Changed::Yes,
                Some(Effect::TourWrite(Setting::HerdrScopeAll))
            )
        );
        assert!(!app.herdr.scoped);
        assert!(listed(&app, "beta"), "every repository is listed now");
    }

    /// A write that failed keeps the card up with the reason and the line to add. The
    /// setting stays applied for the session — the choice was made — and `enter`
    /// acknowledges the sentence and moves on without asking for the write again.
    #[test]
    fn app_tour_failed_write_keeps_the_card_and_enter_moves_past_it() {
        let mut app = with_tour(vec![empty_card(), Card::Keys]);
        app.handle(Action::Tour(TourKey::Down));
        app.handle(Action::Tour(TourKey::Next));
        assert_eq!(
            app.tour_written(Err(
                "could not write /c: nope. Add this line yourself:".to_owned()
            )),
            (Changed::Yes, None)
        );
        let tour = app.tour.as_ref().expect("still up");
        assert_eq!(tour.at, 0, "the same card");
        assert!(tour.failed.is_some());
        assert!(app.hide_empty, "the choice still holds for the session");
        // The choice is spent: the arrows have nothing left to move between.
        assert_eq!(app.handle(Action::Tour(TourKey::Down)), (Changed::No, None));
        assert_eq!(
            app.handle(Action::Tour(TourKey::Next)),
            (Changed::Yes, None),
            "acknowledged, and on to the next card without a second write"
        );
        assert_eq!(app.tour.as_ref().expect("open").at, 1);
    }

    /// A click on a choice row selects it and takes it, in one gesture; a click anywhere
    /// else on the screen under the overlay does nothing at all.
    #[test]
    fn app_tour_click_takes_the_row_it_landed_on() {
        let mut app = with_tour(vec![empty_card()]);
        assert_eq!(
            app.hit(Target::TourRow(1)),
            (
                Changed::Yes,
                Some(Effect::TourWrite(Setting::HideEmptyRepos))
            )
        );
        assert!(app.hide_empty);

        let mut app = with_tour(vec![empty_card()]);
        for target in [
            Target::NavRoot(root("alpha")),
            Target::HeaderAcceptAll,
            Target::FileAccept,
        ] {
            assert_eq!(
                app.hit(target.clone()),
                (Changed::No, None),
                "{target:?} is under the welcome"
            );
        }
        assert!(app.tour.is_some());
        assert!(!app.help);
    }

    /// The footer of a plain card is clickable too — `Target::TourRow(0)` on a card with no
    /// choice rows advances it, so the mouse alone can walk the whole tour.
    #[test]
    fn app_tour_click_on_a_plain_cards_footer_advances_it() {
        let mut app = with_tour(vec![Card::Keys, depth_card()]);
        assert_eq!(app.hit(Target::TourRow(0)), (Changed::Yes, None));
        assert_eq!(app.tour.as_ref().expect("open").at, 1);
    }

    /// Deliverable 8: the depth card's second row asks for the write and changes nothing
    /// the reducer owns. The roots the deeper walk finds are the engine's to deliver.
    #[test]
    fn app_tour_depth_choice_asks_for_the_write_and_changes_no_app_state() {
        let mut app = with_tour(vec![depth_card()]);
        let before = app.clone();
        assert_eq!(
            app.handle(Action::Tour(TourKey::Next)),
            (Changed::Yes, Some(Effect::TourDone)),
            "the first row keeps the default and writes nothing"
        );

        let mut app = with_tour(vec![depth_card()]);
        app.handle(Action::Tour(TourKey::Down));
        assert_eq!(
            app.handle(Action::Tour(TourKey::Next)),
            (Changed::Yes, Some(Effect::TourWrite(Setting::SearchDepth2)))
        );
        assert_eq!(app.hide_empty, before.hide_empty);
        assert_eq!(app.herdr.scoped, before.herdr.scoped);
        assert_eq!(
            app.listed_roots().count(),
            before.listed_roots().count(),
            "no root moved"
        );
        assert_eq!(app.selection, before.selection);
    }

    /// F4: the empty-repository card is built when the tour reaches it, from the list as it
    /// is then. Roots the depth card's rescan found are counted; a list that is no longer
    /// worth the question loses the card.
    #[test]
    fn app_tour_empty_card_is_built_at_the_advance_step() {
        let mut app = with_tour(vec![Card::Keys, Card::Empty { empty: 0, total: 0 }]);
        // Three roots, all with something pending: not worth asking, so no card at all.
        assert_eq!(
            app.handle(Action::Tour(TourKey::Next)),
            (Changed::Yes, Some(Effect::TourDone)),
            "the slot is dropped and the tour ends"
        );

        // The same slot, after twelve empty roots landed.
        let mut app = with_tour(vec![Card::Keys, Card::Empty { empty: 0, total: 0 }]);
        let names: Vec<String> = (0..12).map(|i| format!("e{i:02}")).collect();
        let mut metas: Vec<_> = ["alpha", "beta", "notes"].iter().map(|n| meta(n)).collect();
        metas.extend(names.iter().map(|n| meta(n)));
        app.sync_roots(metas);
        for name in &names {
            app.apply(pile_event(name, Pile::default()));
        }
        assert_eq!(
            app.handle(Action::Tour(TourKey::Next)),
            (Changed::Yes, None)
        );
        assert_eq!(
            app.tour.as_ref().expect("open").card(),
            &Card::Empty {
                empty: 12,
                total: 15
            },
            "counted from the live list, not from when the tour opened"
        );
    }

    /// A click on a row the card does not have is clamped rather than ignored: the hit map
    /// is built from the frame, but a stale press must not choose row 1 on a card whose
    /// row 1 is not there.
    #[test]
    fn app_tour_click_past_the_last_row_is_clamped() {
        let mut app = with_tour(vec![empty_card()]);
        assert_eq!(
            app.hit(Target::TourRow(9)),
            (
                Changed::Yes,
                Some(Effect::TourWrite(Setting::HideEmptyRepos))
            ),
            "clamped to the last row"
        );
    }

    // --- word wrap in the diff pane (Phase 13, deliverable A) ----------------------------

    /// An app on `f1` with the diff focused and the body's geometry already written back,
    /// as a frame would have written it.
    fn wrapping(lines: &[&str], cols: u16, rows: u16) -> App {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        assert_eq!(
            app.apply(pile_event("alpha", alpha_texts(lines))).0,
            Changed::Yes
        );
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::FocusToggle);
        assert_eq!(app.effective_focus(), Focus::Diff);
        app.diff_size = Some((cols, rows));
        app
    }

    /// Heights 1 (header), 1, 3, 3, 3, 5 at eleven columns: the uneven body the no-skip
    /// rules are about.
    fn uneven_app() -> App {
        let app = wrapping(
            &[
                "short",
                &"x".repeat(30),
                &"y".repeat(30),
                &"z".repeat(30),
                &"w".repeat(50),
            ],
            11,
            10,
        );
        assert_eq!(diff_lines(app.view_hunks()), 6);
        app
    }

    #[test]
    fn app_wrap_page_down_never_steps_over_a_line() {
        let mut app = uneven_app();
        // Rows 1 + 1 + 3 + 3 = 8 of ten fit, so lines 0..=3 are whole and the next top is
        // line 4 — not line 0 + 10.
        assert_eq!(app.handle(Action::NavPageDown).0, Changed::Yes);
        assert_eq!(app.diff.scroll, 4);
        // From line 4: 3 + 5 = 8 fits, so the next top is one past line 5, clamped to the
        // last line.
        app.handle(Action::NavPageDown);
        assert_eq!(app.diff.scroll, 5);
        assert_eq!(app.handle(Action::NavPageDown).0, Changed::No, "the end");
    }

    #[test]
    fn app_wrap_page_up_keeps_the_top_line_whole() {
        let mut app = uneven_app();
        app.diff.scroll = 5;
        assert_eq!(app.handle(Action::NavPageUp).0, Changed::Yes);
        // 5 + 3 = 8 of ten rows; 5 + 3 + 3 would be eleven.
        assert_eq!(app.diff.scroll, 4);
        app.handle(Action::NavPageUp);
        assert_eq!(
            app.diff.scroll, 1,
            "3 + 3 + 3 = 9 fits, + 1 more would be ten"
        );
        app.handle(Action::NavPageUp);
        assert_eq!(app.diff.scroll, 0);
        assert_eq!(app.handle(Action::NavPageUp).0, Changed::No);
    }

    #[test]
    fn app_wrap_a_wheel_step_of_three_lands_on_a_line_that_was_drawn() {
        let mut app = uneven_app();
        // Three lines forward from the top is line 3, which was fully drawn, so the wheel
        // takes it as asked.
        assert_eq!(app.handle(Action::ScrollDown(3)).0, Changed::Yes);
        assert_eq!(app.diff.scroll, 3);
        // From line 3 only lines 3 and 4 fit (3 + 3 = 6, + 5 = 11), so a step of three is
        // clamped to one past line 4 rather than skipping line 5's neighbours.
        app.handle(Action::ScrollDown(3));
        assert_eq!(app.diff.scroll, 5);
    }

    #[test]
    fn app_wrap_forward_moves_always_move_at_least_one_line() {
        // A body of four rows caps every line at one row, but even a body too short to
        // hold the top line whole must still make progress.
        let mut app = uneven_app();
        app.diff_size = Some((11, 4));
        let before = app.diff.scroll;
        assert_eq!(app.handle(Action::ScrollDown(1)).0, Changed::Yes);
        assert_eq!(app.diff.scroll, before + 1);
        assert_eq!(app.handle(Action::NavPageDown).0, Changed::Yes);
        assert!(app.diff.scroll > before + 1);
    }

    #[test]
    fn app_wrap_a_selection_over_a_five_row_line_keeps_that_line_whole() {
        let mut app = uneven_app();
        assert_eq!(app.handle(Action::Select).0, Changed::Yes);
        for _ in 0..5 {
            app.handle(Action::NavDown);
        }
        assert_eq!(
            app.sel.expect("live").cursor,
            5,
            "the far end walked to the tall line"
        );
        // Line 5 is five rows; from a top of line 4 its three-row neighbour plus it is
        // eight of ten, so the pane moved to 4 and no further.
        assert_eq!(app.diff.scroll, 4);
        let text = app.copy_payload().expect("a payload");
        assert_eq!(
            String::from_utf8(text).expect("utf-8").lines().count(),
            6,
            "the header the cursor began on down to the tall line, whole"
        );
    }

    #[test]
    fn app_wrap_the_hunk_key_still_puts_the_header_at_the_top_row() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        let mut p = alpha_texts(&["a", &"x".repeat(60)]);
        let second = p.rows[0].hunks[0].clone();
        p.rows[0]
            .hunks
            .push(lastcall_engine::hunks::Hunk { index: 1, ..second });
        app.apply(pile_event("alpha", p));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::FocusToggle);
        app.diff_size = Some((11, 10));
        let offsets = hunk_offsets(app.view_hunks());
        assert_eq!(app.handle(Action::HunkNext).0, Changed::Yes);
        assert_eq!(
            app.diff.scroll, offsets[1],
            "the second header is the top line"
        );
        let table = wrap::layout(app.view_hunks(), app.diff.scroll, 11, 10, true);
        assert_eq!(table[0].line, offsets[1], "and the top row is that header");
    }

    #[test]
    fn app_wrap_toggle_flips_the_flag_and_leaves_the_scroll_alone() {
        let mut app = uneven_app();
        app.diff.scroll = 3;
        assert!(app.wrap, "wrap is on out of the box");
        assert_eq!(app.handle(Action::ToggleWrap), (Changed::Yes, None));
        assert!(!app.wrap);
        assert_eq!(
            app.diff.scroll, 3,
            "the line the reader was on did not move"
        );
        assert_eq!(app.handle(Action::ToggleWrap).0, Changed::Yes);
        assert!(app.wrap);
        assert_eq!(app.diff.scroll, 3);
    }

    #[test]
    fn app_wrap_off_scrolls_a_line_at_a_time_as_it_did_before_the_phase() {
        let mut app = uneven_app();
        app.wrap = false;
        // Ten rows, ten lines: only six exist, so a page down lands on the last.
        assert_eq!(app.handle(Action::NavPageDown).0, Changed::Yes);
        assert_eq!(app.diff.scroll, 5);
        app.handle(Action::NavPageUp);
        assert_eq!(app.diff.scroll, 0, "a body of ten holds all six lines");
    }

    #[test]
    fn app_wrap_resize_forgets_the_measured_body() {
        let mut app = uneven_app();
        assert_eq!(app.diff_size, Some((11, 10)));
        app.handle(Action::Resize(120, 40));
        assert_eq!(app.diff_size, None, "the next frame measures it again");
        // And the fallback is the arithmetic the editor uses, never zero.
        let (cols, rows) = app.diff_body_size();
        assert_eq!(cols as usize, app.diff_cols());
        assert!(cols > 0 && rows > 0);
    }

    /// `y` copies **lines**, so a wrapped one comes off the clipboard whole: no row break
    /// became a newline, and a capped line brings back the text the cap hid rather than
    /// the marker that stood in for it.
    #[test]
    fn app_wrap_copy_takes_a_wrapped_and_a_capped_line_whole() {
        let long = "lorem ipsum ".repeat(30);
        let huge = "x".repeat(4000);
        let mut app = wrapping(&["short", &long, &huge], 40, 12);
        // The wrapped line on its own.
        app.diff.scroll = 2;
        assert_eq!(app.handle(Action::Select).0, Changed::Yes);
        let one = String::from_utf8(app.copy_payload().expect("a payload")).expect("utf-8");
        assert_eq!(one, format!(" {long}\n"), "one line, one newline");
        assert!(!one.contains('…'), "no marker on the clipboard");

        // The capped one: the pane showed nine of its rows, the clipboard has all 4000
        // characters.
        app.sel = None;
        app.diff.scroll = 3;
        app.handle(Action::Select);
        let all = String::from_utf8(app.copy_payload().expect("a payload")).expect("utf-8");
        assert_eq!(all, format!(" {huge}\n"));
        let table = wrap::layout(app.view_hunks(), 3, 40, 12, true);
        assert!(
            table.len() < 12,
            "and the pane did cap it: {} rows",
            table.len()
        );
    }
}
