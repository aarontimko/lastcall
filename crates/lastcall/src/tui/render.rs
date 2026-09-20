//! Screen = f(App, area) (kickoff deliverable 4).
//!
//! `render` paints one frame from `&App` and the frame's own area and returns the `HitMap`
//! the loop resolves mouse presses through. It reads nothing but the app: no clock, no
//! engine, no files. Only the visible window of nav entries and diff lines is built, so a
//! 50 000-line diff costs the same as a 50-line one.
//!
//! `styles` dumps a `Buffer`'s non-default style runs for the snapshot tier, since
//! `TestBackend`'s `Display` shows symbols only.

use std::collections::BTreeSet;

use lastcall_engine::count::with_thousands;
use lastcall_engine::hunks::{EXPAND_LINE_CAP, Hunk, Tag};
use lastcall_engine::ledger::{Flag, iso8601_date};
use lastcall_engine::scan::{Change, Collapsed, Rename, Row};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Widget};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{
    self, AcceptAnswer, AcceptScope, App, Editor, Focus, MIN_SIZE, NAV_MIN_COLS, RootView,
    Selection, Target, annotation_name, diff_lines, hunk_header, plural, restore_question,
};
use super::herdr::{Dot, Link};
use super::input::{Action, MODAL_KEYS};
use super::textbuf::Wrap;
use super::tour::{Kind as TourLine, MIN_COLS as TOUR_MIN_COLS};
use super::wrap;

pub const TOO_SMALL: &str = "too small: 40×10 min";
/// The note modal's box: wide enough for a sentence, narrow enough to sit over the diff.
pub const NOTE_WIDTH: u16 = 60;
/// Text-area height, fixed so the box does not resize while the note is being typed.
pub const NOTE_ROWS: u16 = 5;
/// The insertion point, drawn into the text (see `render_note`).
pub const NOTE_CARET: &str = "▌";
/// The note modal's key line where the terminal cannot tell `Shift-Enter` from `Enter`.
pub const NOTE_KEYS: &str = "⏎ send   ^J newline   Esc cancel";
/// …and where it can (the kitty keyboard protocol; ruling P9).
pub const NOTE_KEYS_ENHANCED: &str = "⏎ send   ⇧⏎ / ^J newline   Esc cancel";

/// The key line the note modal shows, which is a promise: `⇧⏎` appears only on a terminal
/// that reports the enhancement, because everywhere else that key *sends the note*.
pub fn note_keys(enhanced: bool) -> &'static str {
    if enhanced {
        NOTE_KEYS_ENHANCED
    } else {
        NOTE_KEYS
    }
}
pub const PICK_KEYS: &str = "↑↓ choose   ⏎ send   Esc cancel";
/// The snooze modal's key line (Amendment v1.11). Fixed, like the note modal's: these keys
/// are the modal's own and are not in the keymap.
pub const SNOOZE_KEYS: &str = "digits edit   ⏎ snooze   Esc cancel";
pub const NO_SELECTION: &str = "select a file (↑↓ or click) · ? for help";
/// The right pane while the herdr scope verdict is still pending at launch
/// (`HerdrView::scope_pending`): nothing is listed yet, so nothing is selectable.
pub const SCOPE_PENDING: &str = "waiting for herdr scope…";
/// What a flag-only root (no pending rows) shows instead of a file list, in the nav and
/// in the diff pane, so `Enter` has somewhere to land. `<status>` is herdr's own word.
pub fn nothing_pending(status: &str) -> String {
    format!("nothing pending · agent {status}")
}

/// The right pane's opening line for a **selected repo with nothing pending** (§6.7,
/// Amendment v1.9): the reader chose this row, so the pane names the repo rather than
/// repeating the nav's header, and herdr's own status word folds in when there is an agent
/// on it. Its branch line goes beneath, as the root summary's does.
pub fn nothing_pending_in(name: &str, status: Option<&str>) -> String {
    match status {
        Some(s) => format!("nothing pending in {name} · agent {s}"),
        None => format!("nothing pending in {name}"),
    }
}

/// The same line for a nav column too narrow for it: the `nothing pending · ` half is
/// already implied by the branch line's `0 files` above it, while the status word is the
/// only thing on screen that says *why* the root is listed — so that half is what
/// survives a truncation rather than what gets cut (review (b) F9).
pub fn nothing_pending_short(status: &str) -> String {
    format!("agent {status}")
}

/// The help overlay's mouse note (ruling 3): `term::enter` turns mouse capture on, so the
/// terminal's own text selection needs the shift override. It stays now that deliverable 9
/// has landed — shift+drag is still the terminal-native path, and the one that works where
/// OSC 52 does not — with `v`/`y` named beside it. 62 columns, so the note fits inside the
/// overlay at 80 (design review F19).
pub const SELECT_NOTE: &str = "shift+drag selects text (mouse capture is on) · v/y copies";

/// The help overlay's last footer row (Amendment v1.11): the welcome card runs once and
/// then never again, so the one place a reader looks up a key is also the place that says
/// how to get it back. 44 columns, narrower than [`SELECT_NOTE`], so it costs the overlay
/// no width.
pub const TOUR_NOTE: &str = "lastcall tui --tour shows the welcome again";

/// The inline editor's line-number gutter: four columns of number and one for the `▎` that
/// marks a line inside a pending hunk (deliverable 8). `App::EDITOR_GUTTER` is the same
/// number, and the reducer clamps the horizontal scroll to the text width it leaves.
pub const EDITOR_GUTTER: u16 = app::EDITOR_GUTTER as u16;
/// The mark on a line inside a pending hunk.
pub const EDITOR_MARK: &str = "▎";
/// The tint on every line of the hunk the editor was opened at, so the reader can see the
/// region they entered while they type around it (the sponsor's addition to ruling P3).
pub const EDITOR_BAND_BG: Color = Color::Indexed(236);
/// The cursor's line, over the band.
pub const EDITOR_CURSOR_BG: Color = Color::Indexed(238);
/// Drawn in the last column of a row whose content runs off the right edge — the editor
/// does not soft-wrap, so this is how a reader knows there is more.
pub const EDITOR_CLIPPED: &str = "→";
/// The hint line while the editor is open: the two keys that are not text.
pub const EDITOR_HINTS: &str = "^S save   Esc close";

/// The help overlay's newline note where the terminal cannot tell `Shift-Enter` from
/// `Enter` (ruling P9): the guaranteed key, and why the other one is not offered.
pub const NEWLINE_NOTE: &str = "^J is a newline in the note (⇧⏎ needs a kitty-protocol terminal)";
/// …and where it can: the flags were pushed, so the key works and may be named.
pub const NEWLINE_NOTE_ENHANCED: &str = "⇧⏎ or ^J is a newline in the note (kitty protocol on)";

/// The help overlay's newline line. Like [`note_keys`] it is a promise, made in the one
/// place a reviewer looks up a key they have not tried: `⇧⏎` is named only where it works.
/// Which terminals report the protocol is a longer answer than an overlay row, and lives in
/// `docs/dev/tui.md`.
pub fn newline_note(enhanced: bool) -> &'static str {
    if enhanced {
        NEWLINE_NOTE_ENHANCED
    } else {
        NEWLINE_NOTE
    }
}

/// Which pane a screen position belongs to (the wheel scrolls the pane under the pointer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Nav,
    Diff,
}

/// What the last frame put where. Regions are pushed general-to-specific and `at` scans
/// them last-to-first, so a row wins over its pane and the divider wins over both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HitMap {
    pub nav: Option<Rect>,
    pub main: Option<Rect>,
    pub targets: Vec<(Rect, Target)>,
    /// The nav offset this frame used, in **nav lines** (deliverable 9), written back into
    /// [`App::nav_top`] by [`crate::tui::run::Ui::rendered`] so the next frame starts where
    /// this one left off. `None` when the nav was not drawn at all — below
    /// [`NAV_MIN_COLS`], or in a frame with no nav pane — so a narrow window does not reset
    /// an offset the user will see again when it widens.
    pub nav_top: Option<usize>,
    /// The inline editor's **text** area (the gutter already subtracted), when this frame
    /// drew one: what turns a click into a caret position (deliverable 8). Not a `Target`,
    /// because a target says *what* was clicked and this has to answer *where*.
    pub editor: Option<Rect>,
    /// The rectangle the **hunk lines** were drawn into, when this frame drew any
    /// (deliverable 9): what turns a mouse press or drag into a diff line. Like
    /// [`HitMap::editor`] it is a *where*, not a *what*, so it is not a `Target` — and it
    /// is narrower than `Target::DiffBody`, which covers the whole pane including the
    /// expansion header and the empty states.
    pub diff_body: Option<Rect>,
    /// One **diff line index per drawn body row**, top to bottom (Phase 13): the row table
    /// that replaces the old `scroll + (y - rect.y)`, which assumed one line is one row.
    ///
    /// It carries every row the body drew — hunk headers, text rows, the continuation rows
    /// of a wrapped line and the blank separators — so a click on a header still answers
    /// `Target::DiffHunk` and a press on a separator answers what it always did. Every row
    /// of a wrapped line carries that line's index, so clicking any part of it selects the
    /// whole line. Empty when the frame drew no hunks, exactly as `diff_body` is `None`.
    pub diff_rows: Vec<usize>,
}

impl HitMap {
    pub fn at(&self, x: u16, y: u16) -> Option<&Target> {
        let p = Position::new(x, y);
        self.targets
            .iter()
            .rev()
            .find(|(r, _)| r.contains(p))
            .map(|(_, t)| t)
    }

    pub fn pane_at(&self, x: u16, y: u16) -> Option<Pane> {
        let p = Position::new(x, y);
        if self.nav.is_some_and(|r| r.contains(p)) {
            Some(Pane::Nav)
        } else if self.main.is_some_and(|r| r.contains(p)) {
            Some(Pane::Diff)
        } else {
            None
        }
    }
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}
fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}
fn green() -> Style {
    Style::new().fg(Color::Green)
}
fn red() -> Style {
    Style::new().fg(Color::Red)
}
fn focused_border() -> Style {
    Style::new().fg(Color::Cyan)
}

/// Paint one frame; returns the hit map for it.
pub fn render(app: &App, frame: &mut Frame<'_>) -> HitMap {
    let area = frame.area();
    let buf = frame.buffer_mut();
    let mut hits = HitMap::default();
    if area.width < MIN_SIZE.0 || area.height < MIN_SIZE.1 {
        buf.set_stringn(area.x, area.y, TOO_SMALL, area.width as usize, Style::new());
        return hits;
    }

    let header = Rect::new(area.x, area.y, area.width, 1);
    let status = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 2);
    match &app.editor {
        Some(ed) => render_editor_header(ed, buf, header),
        None => render_header(app, buf, header, &mut hits),
    }
    render_status(app, buf, status);

    let nav_visible = area.width >= NAV_MIN_COLS;
    // The editor lives in the diff pane and holds every key, so the focused border follows
    // it there whatever `app.focus` says: the nav is not where the typing goes.
    let focus = if nav_visible && app.editor.is_none() {
        app.focus
    } else {
        Focus::Diff
    };
    let (nav_area, main_area) = if nav_visible {
        let w = app.nav_width.min(area.width.saturating_sub(20));
        (
            Some(Rect::new(body.x, body.y, w, body.height)),
            Rect::new(body.x + w - 1, body.y, body.width - w + 1, body.height),
        )
    } else {
        (None, body)
    };

    let main_block = Block::bordered().border_style(if focus == Focus::Diff {
        focused_border()
    } else {
        Style::new()
    });
    let main_inner = main_block.inner(main_area);
    main_block.render(main_area, buf);
    hits.main = Some(main_inner);
    match &app.editor {
        Some(ed) => render_editor(ed, buf, main_inner, &mut hits),
        None => render_main(app, buf, main_inner, &mut hits),
    }

    if let Some(nav_area) = nav_area {
        let nav_block = Block::bordered().border_style(if focus == Focus::Nav {
            focused_border()
        } else {
            Style::new()
        });
        let nav_inner = nav_block.inner(nav_area);
        nav_block.render(nav_area, buf);
        // The shared column is the divider: fix its junctions and give it the focused style.
        let x = nav_area.right() - 1;
        buf[(x, nav_area.y)].set_symbol("┬");
        buf[(x, nav_area.bottom() - 1)].set_symbol("┴");
        buf.set_style(
            Rect::new(x, nav_area.y, 1, nav_area.height),
            focused_border(),
        );
        hits.nav = Some(nav_inner);
        render_nav(app, buf, nav_inner, &mut hits);
        // The nav stays on screen while the editor is open — the reader keeps the list of
        // what is left to review in front of them — but dimmed, because none of its keys
        // work until the editor closes.
        if app.editor.is_some() {
            buf.set_style(nav_inner, dim());
        }
        hits.targets
            .push((Rect::new(x, body.y, 1, body.height), Target::Divider));
    }

    // Deliverable 9: the copy cue sits over the diff pane, under every modal — a copy is
    // not a question, and it must never hide the one being asked.
    if let Some(cue) = &app.cue {
        render_cue(&cue.text, buf, main_inner);
    }
    if app.help {
        render_help(app, buf, area);
    }
    if app.confirm.is_some() {
        render_confirm(app, buf, area);
    }
    // The note and the picker are the top layer: only one is ever open, and neither can be
    // open with the confirm (a flag never asks).
    if app.note.is_some() {
        render_note(app, buf, area);
    }
    if app.picker.is_some() {
        render_picker(app, buf, area);
    }
    // The snooze modal is the top layer too, and never open beside any of the others: `s`
    // only answers on a repository row, and every modal above swallows it.
    if app.snooze.is_some() {
        render_snooze(app, buf, area);
    }
    // The welcome overlay is above all of them (Amendment v1.11, F7). It opens on the first
    // frame past the launch hold, when none of the modals can be open yet, and its rows go
    // into the hit map **last** so a press on one resolves to the tour and not to whatever
    // the live screen underneath is drawing.
    if app.tour.is_some() {
        render_tour(app, buf, area, &mut hits);
    }
    hits
}

/// `lastcall  <repos> · <files> · <hunks>  <herdr badge>  [Accept All]  ↑ 0.1.1` …
/// `watching <parents>`, or `lastcall  <repos> · checking status…` while the launch hold
/// is on (Design pass D3). The file count carries `+` when any listed root's pile stopped at
/// the row cap; the control is dim when nothing is listed and is the `HeaderAcceptAll`
/// target either way; the badge is the `HeaderHerdr` target.
///
/// Priority when the line is short: the counts, then the notice, then the badge, then the
/// control — `^A` duplicates the control, nothing else says what is being watched or
/// whether herdr is answering. A line with no room for the notice keeps the badge and the
/// control instead of leaving the right half empty.
///
/// **The update notice (Design pass D6)** is the seven columns `↑ 0.1.1`, after the control
/// and before the pad, and it is the **first** thing this ladder drops: a reader who has not
/// asked about releases must not lose the badge or the control to a version number. It is
/// the `HeaderUpdate` target, and a click puts the whole sentence on the status line — the
/// badge's click-to-read pattern. Nothing else in the UI mentions it, so a session that
/// never widens past 80 columns reaches it through `lastcall update --check`.
fn render_header(app: &App, buf: &mut Buffer, area: Rect, hits: &mut HitMap) {
    let listed: Vec<&RootView> = app.listed_roots().collect();
    let files: usize = listed.iter().map(|v| v.rows().len()).sum();
    let hunks: usize = listed
        .iter()
        .flat_map(|v| v.rows().iter())
        .map(|r| r.hunks.len())
        .sum();
    // Design pass D3 / ruling R5: during the launch hold the header says the one count it
    // knows and none of the ones it does not — `0 files · 0 hunks` above a pane reading
    // `discovered 3 repos, checking status…` was the header disagreeing with the pane.
    // `[Accept All]` is dim throughout, as it already is whenever nothing is listed.
    let left = if app.loading.is_some() {
        format!(
            "lastcall  {} · checking status…",
            plural(app.roots.len(), "repo")
        )
    } else {
        format!(
            "lastcall  {} · {} · {}",
            plural(listed.len(), "repo"),
            count_plus(files, app.any_truncated(), "file"),
            plural(hunks, "hunk")
        )
    };
    let control = "[Accept All]";
    let update = app.update.as_ref().map(|v| format!("↑ {v}"));
    let (badge, badge_style) = herdr_badge(app);
    let parents: BTreeSet<String> = app
        .roots
        .values()
        .map(|v| super::app::basename(&v.meta.parent))
        .collect();
    let right = if parents.is_empty() {
        "watching nothing".to_owned()
    } else {
        format!(
            "watching {}",
            parents.into_iter().collect::<Vec<_>>().join(", ")
        )
    };
    let width = area.width as usize;
    let update_width = update.as_ref().map_or(0, |u| u.width());
    let cost = |update_on: bool, badge_on: bool, control_on: bool, notice_on: bool| {
        left.width()
            + if badge_on { 2 + badge.width() } else { 0 }
            + if control_on { 2 + control.width() } else { 0 }
            + if update_on { 2 + update_width } else { 0 }
            + if notice_on { 2 + right.width() } else { 0 }
    };
    // Preference order, first that fits (the last is the unconditional fallback). The
    // update notice is only ever on in the first row, which is what "first dropped" means:
    // with no update to show, `update.is_none()` skips that row and the remaining six are
    // the ladder this header has always had, unchanged.
    let (show_update, show_badge, show_control, show_notice) = [
        (true, true, true, true),
        (false, true, true, true),
        (false, true, false, true),
        (false, false, false, true),
        (false, true, true, false),
        (false, true, false, false),
        (false, false, false, false),
    ]
    .into_iter()
    .filter(|(u, ..)| !u || update.is_some())
    .find(|(u, b, c, n)| cost(*u, *b, *c, *n) <= width)
    .unwrap_or((false, false, false, false));

    let mut used = left.width();
    let mut spans = vec![Span::styled(left, bold())];
    let mut badge_x = None;
    if show_badge {
        spans.push(Span::raw("  "));
        badge_x = Some(area.x + (used + 2) as u16);
        used += 2 + badge.width();
        spans.push(Span::styled(badge.clone(), badge_style));
    }
    let mut control_x = None;
    if show_control {
        spans.push(Span::raw("  "));
        control_x = Some(area.x + (used + 2) as u16);
        used += 2 + control.width();
        spans.push(Span::styled(
            control,
            if listed.is_empty() {
                dim()
            } else {
                Style::new()
            },
        ));
    }
    let mut update_x = None;
    if let (true, Some(update)) = (show_update, &update) {
        spans.push(Span::raw("  "));
        update_x = Some(area.x + (used + 2) as u16);
        used += 2 + update.width();
        spans.push(Span::styled(update.clone(), dim()));
    }
    if show_notice {
        let pad = width.saturating_sub(used + right.width());
        spans.push(Span::raw(format!("{}{right}", " ".repeat(pad))));
    }
    buf.set_line(area.x, area.y, &Line::from(spans), area.width);
    if let (Some(x), Some(update)) = (update_x, &update) {
        hits.targets.push((
            Rect::new(x, area.y, update.width() as u16, 1),
            Target::HeaderUpdate,
        ));
    }
    if let Some(x) = badge_x {
        hits.targets.push((
            Rect::new(x, area.y, badge.width() as u16, 1),
            Target::HeaderHerdr,
        ));
    }
    if let Some(x) = control_x {
        hits.targets.push((
            Rect::new(x, area.y, control.width() as u16, 1),
            Target::HeaderAcceptAll,
        ));
    }
}

/// The header's herdr badge and its style (deliverable 5): `herdr <version>` dim when
/// connected, `herdr ⟳` while reconnecting, `standalone` dim when off or absent, and
/// `standalone: <reason>` when `mode = "on"` made the failure visible.
fn herdr_badge(app: &App) -> (String, Style) {
    match &app.herdr.link {
        Link::Connected { version } => (format!("herdr {version}"), dim()),
        Link::Reconnecting => ("herdr ⟳".to_owned(), Style::new()),
        Link::Off => ("standalone".to_owned(), dim()),
        Link::Standalone { reason } if reason.is_empty() => ("standalone".to_owned(), dim()),
        Link::Standalone { reason } => (format!("standalone: {reason}"), Style::new()),
    }
}

/// `plural`, with `+` after the number when the count is a truncated one (`4+ files`,
/// `10,000+ files`).
fn count_plus(n: usize, plus: bool, noun: &str) -> String {
    if plus {
        format!("{}+ {noun}s", with_thousands(n))
    } else {
        plural(n, noun)
    }
}

/// The status line: a transient status with its age, else the hint line — with the
/// mandatory scope notice (deliverable 8) right-aligned beside whichever of the two is
/// showing, and alone when neither pairing fits.
///
/// The notice is mandatory *while a scope is active* (ruling 1, and the "Scope hiding
/// pending work silently" trap), so a transient status — set at startup, after every
/// accept, on a HEAD change, on a focus verdict — yields the room rather than hiding it:
/// the status text is truncated first and the notice keeps its right-hand column.
fn render_status(app: &App, buf: &mut Buffer, area: Rect) {
    let width = area.width as usize;
    // Amendment v1.11 / design review F6: the scope count and the snooze count share one
    // right-hand notice, and it chooses its own form for the width — `render_status` itself
    // still has no tiers.
    let notice = app.bottom_notice(area.width);
    // The notice needs its own column plus a gap; below that it takes the line alone.
    let notice_room = notice.as_ref().filter(|n| n.width() + 4 <= width);
    let line = match (&app.status, app.status_age()) {
        (Some(s), Some(age)) => {
            let age = format!(" · {age}");
            match (notice_room, &notice) {
                (Some(notice), _) => {
                    let room = (width - notice.width() - 2).saturating_sub(age.width());
                    let text = ellipsize(&s.text, room);
                    let pad = width.saturating_sub(text.width() + age.width() + notice.width());
                    Line::from(vec![
                        Span::raw(text),
                        Span::styled(age, dim()),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(notice.clone(), dim()),
                    ])
                }
                (None, Some(notice)) => Line::from(Span::styled(notice.clone(), dim())),
                (None, None) => {
                    Line::from(vec![Span::raw(s.text.clone()), Span::styled(age, dim())])
                }
            }
        }
        _ => match (notice_room, &notice) {
            (None, None) => Line::from(Span::styled(hints(app, area.width), dim())),
            (None, Some(notice)) => Line::from(Span::styled(notice.clone(), dim())),
            (Some(notice), _) => {
                let room = width - notice.width() - 2;
                let hints = hints(app, room as u16);
                let pad = width.saturating_sub(hints.width() + notice.width());
                Line::from(vec![
                    Span::styled(hints, dim()),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(notice.clone(), dim()),
                ])
            }
        },
    };
    buf.set_line(area.x, area.y, &line, area.width);
}

/// The order hints leave the line when it will not fit, **first to go first** (ruling R4;
/// Phase 9a deliverable 4, and the orchestrator's post-checkpoint note for `hide_empty`'s
/// place). Names are keymap action names, so a rebind moves the key and never the order.
///
/// The shape of it: the diff pane's two extras go first (they are named in the overlay and
/// in `SELECT_NOTE`), then the three whose surface says the same thing another way (`r`
/// refreshes what the watcher does anyway, `Tab` and `w` are visible in the pane layout and
/// the scope notice), then `^A` — which the header's `[Accept All]` duplicates at every
/// width that still draws it — then the toggle, then the herdr jumps, then the accept folds
/// narrowing from the widest to the narrowest, and last the selection's own accept phrase.
/// `help` and `quit` are not in the list at all: they are pinned, so a cut line always says
/// where the rest of the keys are.
const HINT_DROP_ORDER: &[&str] = &[
    // Phase 13, and first of all: the diff pane's wrap toggle is named in the help overlay
    // and the pane itself shows whether it is on, so a full line gives it up before it
    // gives up anything that was on the line before this phase. That is also what keeps
    // every existing 100-column frame unchanged (design review F25).
    "wrap",
    "copy",
    "select",
    "refresh",
    "focus_toggle",
    "scope",
    "snooze",
    "accept_all",
    "hide_empty",
    "undo",
    "jump",
    "ack",
    "accept_file",
    "hunk_next",
    "accept",
];

/// Hints that are **untrue** without a nav pane, and so are never offered below
/// [`NAV_MIN_COLS`] whatever the width arithmetic says: below 70 columns the diff takes the
/// whole body and holds focus, so `Tab focus` toggles nothing, and `v`/`y`/`w`/`r` belong to
/// the same wide-frame set the tiers used to gate together.
const HINT_NAV_ONLY: &[&str] = &["scope", "focus_toggle", "refresh", "select", "copy"];

/// The hint line from the app's own keymap: `↑↓ select  ⏎ open  n/p hunk  <accept>  ^A
/// accept all  t hide empty  Tab focus  r refresh  ? help  q quit`, where `<accept>` follows
/// the selection — `a accept hunk  A accept file` on a file row with hunks in **either**
/// pane, `a/A accept file` on a hunkless file row (binary, collapsed, deleted, unreadable),
/// `A accept group` on a group entry, `A accept all in <root>` on a **non-empty** root entry
/// (Amendment v1.11: `A` is the key that takes a whole entry, and it is how the per-repo
/// fold is told from the header's global one; on an empty repo row neither key accepts
/// anything, so the line does not offer it — verifier (a) F2). With the **diff** focused
/// the first two hints are `↑↓ scroll  ← back` instead, because that is what those keys do
/// there (Design pass D2, ruling R3); the two forms are the same width.
///
/// Ruling R4, the sponsor's own rule: build every applicable hint, try the whole line, and
/// while it does not fit remove one hint at a time from [`HINT_DROP_ORDER`] — no width
/// constants, no all-or-nothing tiers, and `? help  q quit` always the last two on the line.
/// While the confirm modal is open the line is `y confirm  n cancel  q quit`: exactly the
/// keys that work there (the modal's own, fixed, and the keymap's `quit`).
pub fn hints(app: &App, width: u16) -> String {
    let first = |action: &str| app.keys_for(action).first().map(|s| hint_label(s));
    // The editor swallows the keymap, so naming the keymap's keys here would name keys that
    // type themselves. Its own two live in the header as well: the hint line is where every
    // other mode's keys are, and a reader who looks down should not find the nav's.
    if app.editor.is_some() {
        return EDITOR_HINTS.to_owned();
    }
    if app.confirm.is_some() {
        let modal = |name: &str| {
            MODAL_KEYS
                .iter()
                .find(|(n, _)| *n == name)
                .and_then(|(_, specs)| specs.first())
                .map(|s| hint_label(s))
        };
        let items = [
            modal("confirm").map(|k| format!("{k} confirm")),
            modal("cancel").map(|k| format!("{k} cancel")),
            first("quit").map(|k| format!("{k} quit")),
        ];
        return items.into_iter().flatten().collect::<Vec<_>>().join("  ");
    }
    let pair = |a: &str, b: &str| -> Option<String> {
        let (a, b) = (first(a)?, first(b)?);
        if (a.as_str(), b.as_str()) == ("↑", "↓") {
            Some("↑↓".to_owned())
        } else {
            Some(format!("{a}/{b}"))
        }
    };
    let accept = first("accept");
    let accept_file = first("accept_file");
    let scope = app.accept_scope();
    // What `A` takes here. A group entry and a repository row are its alone since
    // Amendment v1.11, so their phrases are built from this and named with its key: the
    // line spells the key that will do the thing it promises.
    let whole = app.accept_file_scope();
    let context = match (&scope, &whole) {
        (Some(AcceptAnswer::Take(AcceptScope::Hunk { .. })), _) => {
            accept.as_ref().map(|k| format!("{k} accept hunk"))
        }
        (Some(AcceptAnswer::Take(AcceptScope::File { .. })), _) => match (&accept, &accept_file) {
            (Some(a), Some(f)) => Some(format!("{a}/{f} accept file")),
            (Some(a), None) => Some(format!("{a} accept file")),
            (None, Some(f)) => Some(format!("{f} accept file")),
            (None, None) => None,
        },
        (_, Some(AcceptScope::Group { .. })) => {
            accept_file.as_ref().map(|k| format!("{k} accept group"))
        }
        // Verifier (a) F2: since v1.9 a repo with nothing pending is a selectable nav row,
        // and `A` on it lands on `nothing to accept`. A hint the line promises has to do
        // something, so the phrase is offered only while the repo has rows.
        (_, Some(AcceptScope::Root(root))) => accept_file
            .as_ref()
            .filter(|_| app.roots.get(root).is_some_and(|v| !v.rows().is_empty()))
            .map(|k| format!("{k} accept all in {}", app.root_name(root))),
        // `Bless` is never what the *selection* covers — it is built by the editor-return
        // path and lives only inside a confirm — so the hint line has nothing to say for it.
        _ => None,
    };
    let file = match &scope {
        Some(AcceptAnswer::Take(AcceptScope::Hunk { .. })) => {
            accept_file.map(|k| format!("{k} accept file"))
        }
        _ => None,
    };
    // (hint, tier): when the line must shrink, tier 4 goes first (`t hide empty`), then
    // tier 3 (the diff pane's two), then tier 2 (`focus`, `refresh`; always below
    // `NAV_MIN_COLS`), then tier 1 (the file and global accept hints).
    // The herdr hints are conditional: `d`/`g` only while the selected root carries a
    // flag, `w` only while a scope is active (deliverable 5's hint ladder).
    // `d` acks a **ready episode** and nothing else, so a blocked root — which is listed,
    // and does carry a dot — must not be offered `d ack`, where the key would do nothing
    // (review (b) F7). `g` is offered for either, because both have a pane to jump to.
    let flag = app.flagged_root().and_then(|r| app.herdr.flag(&r).cloned());
    let ack = flag
        .as_ref()
        .is_some_and(|f| f.ready.is_some())
        .then(|| first("ack").map(|k| format!("{k} ack")))
        .flatten();
    let jump = flag
        .as_ref()
        .is_some_and(|f| f.attention())
        .then(|| first("jump").map(|k| format!("{k} jump")))
        .flatten();
    let undo = app
        .flagged_root()
        .and_then(|r| app.roots.get(&r).map(|v| v.pile.undo))
        .is_some_and(|n| n > 0)
        .then(|| first("undo").map(|k| format!("{k} undo")))
        .flatten();
    // Amendment v1.11, the sponsor's own run (2026-09-14): `s` answers only on a
    // repository row, and there it either snoozes or, on a snoozed repository that
    // `shift-s` is showing, wakes. The label follows the state so the line promises what
    // the key will do; on a file row the key is not offered, because there it refuses.
    let snooze = match &app.selection {
        Some(Selection::Root(root)) => first("snooze").map(|k| {
            let verb = if app
                .roots
                .get(root)
                .is_some_and(|v| app.snoozed(v).is_some())
            {
                "wake"
            } else {
                "snooze"
            };
            format!("{k} {verb}")
        }),
        _ => None,
    };
    let diff = app.effective_focus() == Focus::Diff;
    // Verifier (b) F4, the same rule as `accept all in <root>` above: a hint the line
    // promises has to do something. `v` and `y` work on diff *lines*, and a hunkless entry
    // (binary, collapsed, deleted, unreadable, or a repo row) has none, so on one of those
    // the pair is not offered even though the diff pane holds the focus.
    let selectable = diff && !app.view_hunks().is_empty();
    let select_hint = selectable
        .then(|| first("select").map(|k| format!("{k} select")))
        .flatten();
    let copy_hint = selectable
        .then(|| first("copy").map(|k| format!("{k} copy")))
        .flatten();
    let scope = app
        .herdr
        .scope
        .is_some()
        .then(|| first("scope").map(|k| format!("{k} scope")))
        .flatten();
    // Phase 13 (Amendment v1.14): `c clip` while the pane is wrapping, `c wrap` while it
    // is clipping. Offered only where the pane has hunks, because with none there is
    // nothing to wrap and the key would flip a setting the reader cannot see.
    let wrap_hint = (!app.view_hunks().is_empty())
        .then(|| {
            first("wrap").map(|k| {
                let verb = if app.wrap { "clip" } else { "wrap" };
                format!("{k} {verb}")
            })
        })
        .flatten();
    // §6.7 (Amendment v1.9): the label follows the state, so the line promises what the
    // key will do rather than naming the setting it flips. The help overlay names the key
    // at every width, which is what a line too narrow to carry it falls back on.
    let hide_empty = first("hide_empty").map(|k| {
        let verb = if app.hide_empty { "show" } else { "hide" };
        format!("{k} {verb} empty")
    });
    // Reading order — what the line says when everything fits. It is not the drop order:
    // that is `HINT_DROP_ORDER`, keyed by the same action names, so the two can be read
    // (and changed) independently. `help`/`quit` are last here and absent there.
    // Design pass D2 / ruling R3: tier 0 tells the truth about the **focused** pane. In the
    // diff `↑`/`↓` scroll a line and `⏎` does nothing (`tui.md` Keys), so `↑↓ select
    // ⏎ open` there named one key for what it does not do and another for what it does not
    // do at all — and Phase 8's `v select` put a true `select` beside the untrue one. With
    // the diff focused the line opens `↑↓ scroll  ← back` instead. `back` is the keymap's
    // own action and `←` its arrow spelling, chosen over its first spec (`Esc`) so the two
    // hints read as the pair of arrows they are; a keymap that binds no arrow to `back`
    // falls back to whatever its first key is. The widths are identical to the nav's
    // (`↑↓ scroll` = `↑↓ select` = 9, `← back` = `⏎ open` = 6), so no drop step moves.
    let arrowed = |action: &str| {
        let specs = app.keys_for(action);
        specs
            .iter()
            .map(|s| hint_label(s))
            .find(|l| matches!(l.as_str(), "←" | "→" | "↑" | "↓"))
            .or_else(|| specs.first().map(|s| hint_label(s)))
    };
    let items: Vec<(&str, String)> = [
        (
            "nav",
            pair("nav_up", "nav_down")
                .map(|k| format!("{k} {}", if diff { "scroll" } else { "select" })),
        ),
        (
            "open",
            if diff {
                arrowed("back").map(|k| format!("{k} back"))
            } else {
                first("open").map(|k| format!("{k} open"))
            },
        ),
        (
            // Verifier (b) F4: `n`/`p` walk the hunks of the selected entry, so on a repo
            // row, an empty repo row or a hunkless file they move nothing. Offered only
            // while there is a hunk to move to.
            "hunk_next",
            pair("hunk_next", "hunk_prev")
                .filter(|_| !app.view_hunks().is_empty())
                .map(|k| format!("{k} hunk")),
        ),
        ("accept", context),
        ("accept_file", file),
        (
            // Verifier (b) F4: with nothing pending anywhere `^A` lands on `nothing to
            // accept`. `counts_of` is what the accept itself would cover, so the hint and
            // the key agree by construction.
            "accept_all",
            first("accept_all")
                .filter(|_| app.counts_of(&AcceptScope::All).files > 0)
                .map(|k| format!("{k} accept all")),
        ),
        // Amendment v1.11 (design review F14): offered only while the selected repository
        // has something to undo, which the pile says (`Pile::undo`) — the hint line never
        // promises a key that would land on `nothing to undo`.
        ("undo", undo),
        ("snooze", snooze),
        ("ack", ack),
        ("jump", jump),
        ("hide_empty", hide_empty),
        ("scope", scope),
        (
            "focus_toggle",
            first("focus_toggle").map(|k| format!("{k} focus")),
        ),
        ("refresh", first("refresh").map(|k| format!("{k} refresh"))),
        // Phase 8 deliverable 9: the diff pane's own two keys. They are still the first two
        // off the line (`HINT_DROP_ORDER`), so no narrower frame loses a hint it used to
        // have — and the help overlay and its mouse note name them at every width.
        ("select", select_hint),
        ("copy", copy_hint),
        // Phase 13, beside the diff pane's other two: the verb follows the state, as
        // `hide_empty`'s and `snooze`'s do, so the line promises what the key will do
        // rather than naming the setting it flips. Offered only where there are hunks to
        // wrap, and **first** in `HINT_DROP_ORDER`, so a full line at 100 columns is
        // exactly the line it was before this phase.
        ("wrap", wrap_hint),
        ("help", first("help").map(|k| format!("{k} help"))),
        ("quit", first("quit").map(|k| format!("{k} quit"))),
    ]
    .into_iter()
    .filter_map(|(name, hint)| hint.map(|h| (name, h)))
    .collect();

    let mut dropped: Vec<&str> = if width >= NAV_MIN_COLS {
        Vec::new()
    } else {
        HINT_NAV_ONLY.to_vec()
    };
    let join = |dropped: &[&str]| -> String {
        items
            .iter()
            .filter(|(name, _)| !dropped.contains(name))
            .map(|(_, hint)| hint.as_str())
            .collect::<Vec<_>>()
            .join("  ")
    };
    let mut line = join(&dropped);
    // One hint at a time, in the fixed order, until it fits. When even the last of them is
    // gone the line is the four that are never dropped (`↑↓ select  ⏎ open  ? help  q quit`,
    // 33 columns) — inside `MIN_SIZE`'s 40, so the promise that `? help` is on every legal
    // frame is arithmetic, not luck.
    for name in HINT_DROP_ORDER {
        if line.width() <= width as usize {
            break;
        }
        if dropped.contains(name) {
            continue;
        }
        dropped.push(name);
        line = join(&dropped);
    }
    line
}

/// `key_label` with control keys as `^X`, the hint line's compact spelling.
fn hint_label(spec: &str) -> String {
    match spec.strip_prefix("ctrl-") {
        Some(rest) => format!("^{}", rest.to_uppercase()),
        None => key_label(spec),
    }
}

// ---- nav ---------------------------------------------------------------------------------

struct NavLine<'a> {
    line: Line<'a>,
    target: Option<Target>,
    selected: bool,
}

fn render_nav(app: &App, buf: &mut Buffer, area: Rect, hits: &mut HitMap) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let mut lines: Vec<NavLine> = Vec::new();
    let mut selected_at: Option<usize> = None;
    // (line index, dot width, root): the dot's own hit rect, pushed once the line's final
    // screen row is known (below).
    let mut dots: Vec<(usize, u16, std::path::PathBuf)> = Vec::new();
    let mut first = true;
    for (path, view) in &app.roots {
        if !app.is_listed(view) {
            continue;
        }
        if !first {
            lines.push(NavLine {
                line: Line::from(Span::styled("─".repeat(width), dim())),
                target: None,
                selected: false,
            });
        }
        first = false;
        let sel = Some(Selection::Root(path.clone()));
        let is_sel = app.selection == sel;
        let mut spans = Vec::new();
        // The dot sits before the bold name, with the agent count when there is more than
        // one; clicking it acks (`RootDot`), clicking the name selects as it always did.
        if let Some(dot) = app.herdr.dot(path) {
            let agents = app.herdr.flag(path).map(|f| f.agents).unwrap_or(1);
            let text = match agents {
                0 | 1 => format!("{} ", dot_glyph(dot)),
                n => format!("{}{n} ", dot_glyph(dot)),
            };
            dots.push((lines.len(), text.width() as u16, path.clone()));
            spans.push(Span::styled(text, dot_style(dot)));
        }
        // §6.7 (Amendment v1.9): a repo with nothing pending is on the nav like any other,
        // told apart by being dim from the name down — bold-dim, so it still reads as a
        // repo heading and not as a file row.
        let empty = view.rows().is_empty();
        let name_style = if empty {
            bold().add_modifier(Modifier::DIM)
        } else {
            bold()
        };
        // A watched folder's name can carry folders above it (`repo/z_ignore`), so a name
        // wider than the column is cut at the **head**: the tail is the folder itself,
        // which is the half that tells two rows apart.
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let name = ellipsize_head(&view.meta.name, width.saturating_sub(used));
        spans.push(Span::styled(name.clone(), name_style));
        let remote = view.meta.remote.as_deref().filter(|_| app.show_remote);
        if let Some(remote) = remote {
            let budget = width.saturating_sub(name.width() + 2);
            if budget >= 2 {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(ellipsize(remote, budget), dim()));
            }
        }
        for label in [view.meta.badge_label(), view.meta.in_progress_label()]
            .into_iter()
            .flatten()
        {
            spans.push(Span::raw(format!("  {label}")));
        }
        if is_sel {
            selected_at = Some(lines.len());
        }
        lines.push(NavLine {
            line: Line::from(spans),
            target: Some(Target::NavRoot(path.clone())),
            selected: is_sel,
        });
        let branch = format!(
            "  {} · {}",
            view.meta.branch_label(),
            count_plus(view.rows().len(), view.pile.omitted > 0, "file")
        );
        let mut branch_spans = vec![if empty {
            Span::styled(branch, dim())
        } else {
            Span::raw(branch)
        }];
        // Amendment v1.11: a snoozed repository is only on the nav because `shift-s` is
        // showing it (or its agent wants attention), so the line says why it is here.
        if let Some(until) = app.snoozed(view) {
            branch_spans.push(Span::styled(
                format!(" · snoozed until {}", iso8601_date(until)),
                dim(),
            ));
        }
        lines.push(NavLine {
            line: Line::from(branch_spans),
            target: None,
            selected: false,
        });
        if let Some(status) = empty
            .then(|| app.herdr.flag(path).map(|f| f.status.as_str()))
            .flatten()
        {
            // A flagged repo with nothing pending keeps its third line: herdr's status word
            // is the only thing on the nav that says what the agent is doing. A plain empty
            // repo is the name-and-branch row the sponsor asked for and nothing more.
            let full = format!("  {}", nothing_pending(status));
            let text = if full.width() <= width {
                full
            } else {
                format!("  {}", nothing_pending_short(status))
            };
            lines.push(NavLine {
                line: Line::from(Span::styled(text, dim())),
                target: Some(Target::NavRoot(path.clone())),
                selected: false,
            });
        }
        for row in view.rows() {
            let sel = Selection::Row(path.clone(), row.path.clone());
            let is_sel = app.selection.as_ref() == Some(&sel);
            if is_sel {
                selected_at = Some(lines.len());
            }
            lines.push(NavLine {
                line: nav_row_line(row, app.full_paths, width),
                target: Some(Target::NavRow(path.clone(), row.path.clone())),
                selected: is_sel,
            });
        }
        for group in &view.groups {
            let sel = Selection::Group(path.clone(), group.kind);
            let is_sel = app.selection.as_ref() == Some(&sel);
            if is_sel {
                selected_at = Some(lines.len());
            }
            lines.push(NavLine {
                line: Line::from(format!(
                    "  {} · {}",
                    annotation_name(group.kind),
                    plural(group.paths.len(), "file")
                )),
                target: Some(Target::NavGroup(path.clone(), group.kind)),
                selected: is_sel,
            });
        }
    }

    // Deliverable 9: the offset persists across frames. Before, it was derived from the
    // selection alone, so every frame with the selection in view snapped back to line 0 and
    // a mouse-only reader could never see past the first screenful of a long nav.
    //
    // The rules, in order: clamp what the last frame left (the list may have shrunk under
    // it), then scroll the minimum that brings the selected line back into
    // `[top, top + rows)` — above, the line becomes the top; below, the bottom.
    let rows = area.height as usize;
    let max_top = lines.len().saturating_sub(rows);
    let mut offset = app.nav_top.min(max_top);
    if let Some(i) = selected_at {
        if i < offset {
            offset = i;
        } else if i >= offset + rows {
            offset = i + 1 - rows;
        }
    }
    hits.nav_top = Some(offset);
    for (i, entry) in lines.iter().enumerate().skip(offset).take(rows) {
        let y = area.y + (i - offset) as u16;
        let row_rect = Rect::new(area.x, y, area.width, 1);
        buf.set_line(area.x, y, &entry.line, area.width);
        if entry.selected {
            buf.set_style(row_rect, Style::new().add_modifier(Modifier::REVERSED));
        }
        if let Some(t) = &entry.target {
            hits.targets.push((row_rect, t.clone()));
        }
        // The dot wins over the row it sits on (`at` scans last-to-first).
        if let Some((_, w, root)) = dots.iter().find(|(line, _, _)| *line == i) {
            hits.targets
                .push((Rect::new(area.x, y, *w, 1), Target::RootDot(root.clone())));
        }
    }
}

/// The glyph for a dot (deliverable 5).
fn dot_glyph(dot: Dot) -> char {
    match dot {
        Dot::Ready { .. } => '\u{2691}',
        Dot::Blocked | Dot::Working => '\u{25cf}',
        Dot::Unknown => '\u{b7}',
    }
}

/// Bright while the flag is unacked, dim once acked (herdr still says `done`, so the flag
/// stays — only its weight changes); red blocked, yellow working, dim unknown.
fn dot_style(dot: Dot) -> Style {
    match dot {
        Dot::Ready { acked: false } => bold(),
        Dot::Ready { acked: true } => dim(),
        Dot::Blocked => red(),
        Dot::Working => Style::new().fg(Color::Yellow),
        Dot::Unknown => dim(),
    }
}

fn letter(change: Change) -> char {
    match change {
        Change::Modified => 'M',
        Change::Added => 'A',
        Change::Deleted => 'D',
        Change::Mode => 'X',
        Change::Typechange => 'T',
        Change::Unreadable => '?',
    }
}

/// `  M name ⊟ ⚑  [conflict]  +3 −1  [upstream]`, the name ellipsized so the rest fits.
fn nav_row_line(row: &Row, full_paths: bool, width: usize) -> Line<'static> {
    let path = row.path_lossy();
    let name = if full_paths {
        path.clone()
    } else {
        path.rsplit('/').next().unwrap_or(&path).to_owned()
    };
    let mut markers = String::new();
    if row.collapsed.is_some() {
        markers.push_str(" ⊟");
    }
    // One flag is a bare glyph; several carry the count, so `⚑2` says at a glance that the
    // row has more than one note on it without opening the diff.
    match row.flags.len() {
        0 => {}
        1 => markers.push_str(" ⚑"),
        n => markers.push_str(&format!(" ⚑{n}")),
    }
    let conflict = if row.conflicted { "  [conflict]" } else { "" };
    let counts_added = format!("+{}", with_thousands(row.added));
    let counts_deleted = format!("−{}", with_thousands(row.deleted));
    let annotation = row
        .annotation
        .map(|a| format!("  [{}]", annotation_name(a)))
        .unwrap_or_default();
    let fixed = 4 // "  M "
        + markers.width()
        + conflict.width()
        + 2
        + counts_added.width()
        + 1
        + counts_deleted.width()
        + annotation.width();
    let name = ellipsize(&name, width.saturating_sub(fixed).max(1));
    let mut spans = vec![Span::raw(format!(
        "  {} {name}{markers}{conflict}  ",
        letter(row.change)
    ))];
    spans.push(Span::styled(counts_added, green()));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(counts_deleted, red()));
    if !annotation.is_empty() {
        spans.push(Span::styled(annotation, dim()));
    }
    Line::from(spans)
}

// ---- main --------------------------------------------------------------------------------

fn render_main(app: &App, buf: &mut Buffer, area: Rect, hits: &mut HitMap) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    hits.targets.push((area, Target::DiffBody));
    let mut lines: Vec<Line> = Vec::new();
    match &app.selection {
        None => {
            if let Some(loading) = &app.loading {
                // The launch hold (`Loading`): a static line, then after a second the
                // counter and a ✓ per reported root — the slow one is the one without.
                // Design pass D3 / ruling R5: **repos** is the noun wherever a count is
                // shown here and in the empty states (`root` stays the config and CLI
                // word), the counter line does not repeat it (`1 of 3 checked`), and the
                // header agrees with the pane instead of reading `0 repos · 0 files ·
                // 0 hunks` above `discovered 3 repos`. The pane is also the hold's only
                // clock: `run.rs` no longer puts `scanning N roots…` on the status line.
                lines.push(Line::from(format!(
                    "discovered {}, checking status…",
                    plural(app.roots.len(), "repo")
                )));
                let counting = loading.counting(app.now);
                if counting {
                    // Design pass D5 / ruling R7: with every root reported and only the
                    // herdr scope verdict outstanding, the same counter carries the
                    // holding clause where `so far · Ss` was. The clock is the scans', so
                    // it stops when they do — the sentence is what is being waited on now.
                    let tail = if loading.scanned {
                        format!(
                            "{} pending · {SCOPE_PENDING}",
                            plural(loading.files(), "file")
                        )
                    } else {
                        format!(
                            "{} pending so far · {}s",
                            plural(loading.files(), "file"),
                            app.now.duration_since(loading.started).as_secs()
                        )
                    };
                    lines.push(Line::from(Span::styled(
                        format!(
                            "{} of {} checked · {tail}",
                            with_thousands(loading.checked.len()),
                            with_thousands(app.roots.len()),
                        ),
                        dim(),
                    )));
                }
                for view in app.roots.values() {
                    // Design pass D4 / ruling R6: the tick takes the row's own two-space
                    // indent instead of following the branch label, so every tick lands in
                    // the same column and the root still being scanned is the one gap in
                    // it — a glance, not a read down a ragged edge. Nothing moves when the
                    // ticks appear (the indent is already there), so the one-calm-frame
                    // rule for a fast launch still holds.
                    let tick = if counting && loading.checked.contains_key(&view.meta.path) {
                        "✓ "
                    } else {
                        "  "
                    };
                    lines.push(Line::from(format!(
                        "{tick}{}  {}",
                        view.meta.name,
                        view.meta.branch_label()
                    )));
                }
            } else if app.herdr.scope_pending {
                // Deliberately not "nothing pending": the piles may be in and held back.
                // With roots this is the hold's frame above (D5 / R7); the standalone line
                // is what is left for a launch with no roots at all to hold.
                lines.push(Line::from(Span::styled(SCOPE_PENDING, dim())));
            } else if app.listed_roots().all(|v| v.rows().is_empty()) {
                // Nothing on the nav has a file row — including the case where the nav is
                // empty. This pane is then the **empty state**, not a prompt: since
                // Amendment v1.9 the ordinary all-clean launch lists three empty repo
                // rows and selects none of them, and `select a file (↑↓ or click)` over
                // three `0 files` rows invites choosing a file that does not exist
                // (verifier (a) F5).
                if let Some(scope) = app.herdr.active_scope() {
                    // Under a scope, the roots it hides are not "nothing pending": name the
                    // scope, list what it covers, and say how many it hides. Gated on the
                    // scope alone — while it was gated on `scoped_out() >= 1` a scope that
                    // hid only empty repos fell through to the global text, which then
                    // listed the very repos the scope was hiding (verifier (a) F6).
                    lines.push(Line::from(format!("nothing pending in {}", scope.label)));
                    for view in app
                        .roots
                        .values()
                        .filter(|v| scope.roots.contains(&v.meta.path))
                    {
                        lines.push(Line::from(format!(
                            "  {}  {}",
                            view.meta.name,
                            view.meta.branch_label()
                        )));
                    }
                    lines.push(Line::from(Span::styled(
                        format!("{} hidden (w shows all)", plural(app.scoped_out(), "repo")),
                        dim(),
                    )));
                } else {
                    lines.push(Line::from(format!(
                        "nothing pending across {}",
                        plural(app.roots.len(), "repo")
                    )));
                    for view in app.roots.values() {
                        let mut text =
                            format!("  {}  {}", view.meta.name, view.meta.branch_label());
                        for label in [view.meta.badge_label(), view.meta.in_progress_label()]
                            .into_iter()
                            .flatten()
                        {
                            text.push_str("  ");
                            text.push_str(&label);
                        }
                        lines.push(Line::from(text));
                    }
                }
            } else {
                lines.push(Line::from(Span::styled(NO_SELECTION, dim())));
            }
        }
        Some(Selection::Root(root)) => {
            if let Some(view) = app.roots.get(root) {
                let branch = format!(
                    "  {} · {}",
                    view.meta.branch_label(),
                    count_plus(view.rows().len(), view.pile.omitted > 0, "file")
                );
                // §6.7 (Amendment v1.9): an empty repo is a selectable nav row now, so its
                // pane answers "which repo, and why is it empty?" on the first line and
                // carries the branch beneath — the same two facts the summary shows, said
                // the other way round.
                let mut spans = if view.rows().is_empty() {
                    let status = app.herdr.flag(root).map(|f| f.status.as_str());
                    lines.push(Line::from(Span::styled(
                        nothing_pending_in(&view.meta.name, status),
                        bold(),
                    )));
                    vec![Span::styled(branch, dim())]
                } else {
                    vec![
                        Span::styled(view.meta.name.clone(), bold()),
                        Span::raw(branch),
                    ]
                };
                for label in [view.meta.badge_label(), view.meta.in_progress_label()]
                    .into_iter()
                    .flatten()
                {
                    spans.push(Span::raw(format!("  {label}")));
                }
                lines.push(Line::from(spans));
                // R5: where this root actually is, under the two lines that say what it
                // is. A name can be a folder name two repositories share, and a watched
                // folder's name is a name the configuration chose, so the answer to
                // "which one is this?" has to be on the pane rather than in the reader's
                // memory. Dim, and for a root of any kind — an empty one too.
                lines.push(Line::from(Span::styled(
                    view.meta.path_shown.clone(),
                    dim(),
                )));
                push_notices(&mut lines, view.notices());
                for row in view.rows() {
                    lines.push(nav_row_line(row, true, usize::MAX));
                }
            }
        }
        Some(Selection::Group(root, kind)) => {
            if let Some(group) = app.roots.get(root).and_then(|v| v.group(*kind)) {
                lines.push(Line::from(vec![
                    Span::styled(annotation_name(*kind).to_owned(), bold()),
                    Span::raw(format!(" · {}", plural(group.paths.len(), "file"))),
                ]));
                for p in &group.paths {
                    lines.push(Line::from(format!("  {}", String::from_utf8_lossy(p))));
                }
            }
        }
        Some(Selection::Row(root, path)) => {
            if let Some((view, row)) = app
                .roots
                .get(root)
                .and_then(|v| v.row(path).map(|r| (v, r)))
            {
                let control = format!("[{} accept file]", control_key(app, "accept_file"));
                let restore = format!("[{} restore file]", control_key(app, "restore_file"));
                // The header is built knowing what will be right-aligned after it, so the
                // flag marker takes the leftover and not the controls' room.
                let used = row_header(row, 0).width();
                let mut header = row_header(
                    row,
                    marker_budget(area.width, used, &[control.as_str(), restore.as_str()]),
                );
                // Two controls here, not three: `[m flag]` is a hunk control, and the nav's
                // own `m` (which flags the file) has no header line to hang off.
                let at = right_align_run(
                    &mut header,
                    &[control.as_str(), restore.as_str()],
                    area.width,
                    dim(),
                );
                let (at_accept, at_restore) = (at[0], at[1]);
                if let Some(x) = at_accept {
                    hits.targets.push((
                        Rect::new(area.x + x, area.y, control.width() as u16, 1),
                        Target::FileAccept,
                    ));
                }
                if let Some(x) = at_restore {
                    hits.targets.push((
                        Rect::new(area.x + x, area.y, restore.width() as u16, 1),
                        Target::FileRestore,
                    ));
                }
                lines.push(header);
                // The row's own `<path>: …` notice is the body of an unreadable row, so the
                // dimmed list above it carries only the root's other notices.
                let own = format!("{}: ", row.path_lossy());
                let others: Vec<String> = view
                    .notices()
                    .iter()
                    .filter(|n| !n.starts_with(&own))
                    .cloned()
                    .collect();
                push_notices(&mut lines, &others);
                // Header and notices are bounded by the pane: a root with more notices than
                // rows must not write past the buffer.
                let fixed = lines.len().min(area.height as usize);
                for (i, line) in lines.iter().take(fixed).enumerate() {
                    buf.set_line(area.x, area.y + i as u16, line, area.width);
                }
                let rest = Rect::new(
                    area.x,
                    area.y + fixed as u16,
                    area.width,
                    area.height - fixed as u16,
                );
                render_row_body(app, buf, rest, view, row, &path.clone(), hits);
                return;
            }
        }
    }
    for (i, line) in lines.iter().take(area.height as usize).enumerate() {
        buf.set_line(area.x, area.y + i as u16, line, area.width);
    }
}

/// `editing <path> · line <n>/<total> · ^S save   Esc close` — the header row while the
/// inline editor is open (deliverable 8).
///
/// It replaces the app header wholesale rather than sitting beside it: the counts, the
/// herdr badge and `[Accept All]` all describe a review the reader has stepped out of, and
/// the `[Accept All]` control in particular is a click target that must not be live while
/// a buffer is open. Red until the next key when a save was refused — the file on disk is
/// not what is on screen, and that is worth more than one line of status.
fn render_editor_header(ed: &Editor, buf: &mut Buffer, area: Rect) {
    // The keys live in the hint line, where every other key hint in the TUI lives; the
    // header is the answer to "what am I in, and where in it?". Design pass D9 / ruling
    // R10: only the **path** gives, and it gives from the head. A tail cut took the
    // `· unsaved` first and then the `line N/M` — the wrong end to lose, since the
    // position is the part that changes as you type and `unsaved` is the part that says
    // the file on disk is not what is on screen. `· unsaved` is reserved at every width
    // whether or not the buffer is dirty, so the header does not shift under the reader on
    // the first keystroke. The path's budget is what is left after all of that, and
    // `ellipsize_head` cuts inside the basename rather than overflow it, so the outer
    // `ellipsize` below is a last resort for frames narrower than the fixed parts.
    let position = ed.buf.position_label();
    let path = String::from_utf8_lossy(&ed.rendered.path);
    const UNSAVED: &str = " · unsaved";
    let budget = (area.width as usize)
        .saturating_sub("editing ".width() + " · ".width() + position.width() + UNSAVED.width());
    let text = format!(
        "editing {} · {position}{}",
        ellipsize_head(&path, budget),
        if ed.buf.dirty() { UNSAVED } else { "" },
    );
    let style = if ed.alarm { red() } else { bold() };
    buf.set_stringn(
        area.x,
        area.y,
        ellipsize(&text, area.width as usize),
        area.width as usize,
        style,
    );
}

/// The inline editor in the diff pane: a five-column gutter, then the file (deliverable 8).
///
/// The gutter carries the line number and, on every line inside a pending hunk, a `▎` — so
/// the reader can see the rest of the agent's work while they type in one part of it. The
/// hunk they *entered* is tinted whole, its marks bold: that band is the answer to "which
/// change was I looking at?", and it grows as they type inside it.
///
/// Nothing here scrolls the buffer. `TextBuf::view` draws the window the reducer clamped
/// after the last key ([`App::clamp_editor`]), because a renderer that moved what it draws
/// would make the frame depend on when it was drawn.
fn render_editor(ed: &Editor, buf: &mut Buffer, area: Rect, hits: &mut HitMap) {
    if area.width <= EDITOR_GUTTER || area.height == 0 {
        return;
    }
    let text_w = area.width - EDITOR_GUTTER;
    let text = Rect::new(area.x + EDITOR_GUTTER, area.y, text_w, area.height);
    hits.editor = Some(text);
    let view = ed.buf.view(area.height as usize, text_w as usize);
    for (i, row) in view.rows.iter().enumerate() {
        let n = view.first_line + i;
        let (marked, in_band) = (ed.marked(n), ed.in_band(n));
        let mark_style = match (marked, in_band) {
            (true, true) => bold(),
            (true, false) => Style::new(),
            (false, _) => dim(),
        };
        let mut line = Line::from(vec![
            Span::styled(format!("{:>4}", n + 1), dim()),
            Span::styled(
                if marked { EDITOR_MARK } else { " " }.to_owned(),
                mark_style,
            ),
            Span::raw(row.clone()),
        ]);
        let row_style = match (n == ed.buf.cursor.line, in_band) {
            (true, _) => Style::new().bg(EDITOR_CURSOR_BG),
            (false, true) => Style::new().bg(EDITOR_BAND_BG),
            (false, false) => Style::new(),
        };
        band(&mut line, area.width, row_style);
        let y = area.y + i as u16;
        buf.set_line(area.x, y, &line, area.width);
        // The right edge, after the line is down: a row that runs off it says so, because
        // the editor does not wrap and the rest of the line is one `End` away.
        if view.clipped.get(i) == Some(&true) && text_w > 0 {
            buf[(text.right() - 1, y)]
                .set_symbol(EDITOR_CLIPPED)
                .set_style(row_style.patch(dim()));
        }
    }
    // The caret, reversed rather than left to the terminal's own cursor: the frame is the
    // only thing a snapshot and a PTY scene can see.
    let (cy, cx) = view.caret;
    if (cy as u16) < area.height && (cx as u16) < text_w {
        let cell = &mut buf[(text.x + cx as u16, area.y + cy as u16)];
        cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
    }
}

fn push_notices(lines: &mut Vec<Line<'static>>, notices: &[String]) {
    for n in notices {
        lines.push(Line::from(Span::styled(n.clone(), dim())));
    }
}

/// `<path>  <letter>  +a −d  [annotation]  (renamed from <old> 90%)`
fn row_header(row: &Row, budget: usize) -> Line<'static> {
    let mut spans = vec![
        Span::styled(row.path_lossy(), bold()),
        Span::raw(format!("  {}  ", letter(row.change))),
        Span::styled(format!("+{}", with_thousands(row.added)), green()),
        Span::raw(" "),
        Span::styled(format!("−{}", with_thousands(row.deleted)), red()),
    ];
    if row.conflicted {
        spans.push(Span::raw("  [conflict]"));
    }
    if let Some(a) = row.annotation {
        spans.push(Span::styled(format!("  [{}]", annotation_name(a)), dim()));
    }
    match &row.rename {
        Some(Rename::From { from, similarity }) => spans.push(Span::raw(format!(
            "  (renamed from {} {similarity}%)",
            String::from_utf8_lossy(from)
        ))),
        Some(Rename::To { to, similarity }) => spans.push(Span::raw(format!(
            "  (renamed to {} {similarity}%)",
            String::from_utf8_lossy(to)
        ))),
        None => {}
    }
    if let Some(f) = row.flags.first() {
        // Bounded, and the first line only. A note is whatever the reviewer typed — it can
        // be a paragraph, and it can contain newlines — and this is a one-line header with
        // the file's controls right-aligned after it. `budget` is what is left once those
        // are reserved, so the marker never costs the reader a control.
        if let Some(text) = flag_marker(&f.note, budget) {
            spans.push(Span::raw(text));
        }
    }
    Line::from(spans)
}

/// `  ⚑ <the note's first line>` in at most `budget` columns, or `None` when there is not
/// enough room to say anything. Shared by the file header and the hunk header so both mark
/// a flag the same way.
fn flag_marker(note: &str, budget: usize) -> Option<String> {
    const PREFIX: usize = 4; // "  ⚑ "
    if budget < PREFIX + 2 {
        return None;
    }
    let first = note.lines().next().unwrap_or("");
    if first.is_empty() {
        return None;
    }
    Some(format!("  ⚑ {}", ellipsize(first, budget - PREFIX)))
}

/// What is left of `width` for a flag marker on a line already holding `used` columns and
/// about to get a right-aligned run of `controls` (which `right_align` pads by two).
fn marker_budget(width: u16, used: usize, controls: &[&str]) -> usize {
    let run = controls.join(" ").width() + 2;
    (width as usize).saturating_sub(used + run)
}

fn render_row_body(
    app: &App,
    buf: &mut Buffer,
    area: Rect,
    view: &RootView,
    row: &Row,
    path: &[u8],
    hits: &mut HitMap,
) {
    if area.height == 0 {
        return;
    }
    let single = |text: String, style: Style| Line::from(Span::styled(text, style));
    match (row.change, row.collapsed) {
        (Change::Unreadable | Change::Typechange, _) => {
            let prefix = format!("{}: ", String::from_utf8_lossy(path));
            let text = view
                .notices()
                .iter()
                .find(|n| n.starts_with(&prefix))
                .cloned()
                .unwrap_or_else(|| {
                    if row.change == Change::Unreadable {
                        "unreadable".to_owned()
                    } else {
                        "typechange".to_owned()
                    }
                });
            buf.set_line(area.x, area.y, &single(text, dim()), area.width);
            return;
        }
        (_, Some(kind)) => {
            // The collapsed header, and under it the expansion when `e` fetched one
            // (Phase 6 deliverable 4). A binary row shows no control: there is nothing
            // text-shaped to expand, so the key and the click are both no-ops there.
            let name = match kind {
                Collapsed::Glob => "glob",
                Collapsed::Binary => "binary",
                Collapsed::Size => "size",
                // The label carries the limit the config set, so the line says why the
                // content is missing without the reader going looking for the number.
                Collapsed::Unread { .. } => "unread",
            };
            let tail = if kind == Collapsed::Binary {
                " · not expandable"
            } else {
                ""
            };
            // A mode-only change on a collapsed row has no content hunks to count, so the
            // header is where the change is named (verifier (a) F5 / (b) F5).
            let mode = match (&row.baseline, &row.current) {
                (Some(b), Some(c)) if b.mode != c.mode => {
                    format!(" · mode {} → {}", b.mode.as_str(), c.mode.as_str())
                }
                _ => String::new(),
            };
            // A file too large to read has no counts to print and nothing to expand: the
            // line is the label alone, so the row says what happened and stops there.
            let mut line = match kind {
                Collapsed::Unread { over_bytes } => single(
                    format!(
                        "not read (over {}){mode}",
                        lastcall_engine::scan::size_limit_label(over_bytes)
                    ),
                    dim(),
                ),
                _ => single(
                    format!(
                        "collapsed ({name}) · +{} −{}{tail}{mode}",
                        with_thousands(row.added),
                        with_thousands(row.deleted)
                    ),
                    dim(),
                ),
            };
            if !matches!(kind, Collapsed::Binary | Collapsed::Unread { .. }) {
                let control = format!("[{} expand]", control_key(app, "expand"));
                if let Some(x) = right_align(&mut line, &control, area.width, dim()) {
                    hits.targets.push((
                        Rect::new(area.x + x, area.y, control.width() as u16, 1),
                        Target::Expand,
                    ));
                }
            }
            buf.set_line(area.x, area.y, &line, area.width);
            let Some(exp) = app.expansion() else {
                return;
            };
            // The cap footer owns the last line whenever it has something to say, so a
            // truncated expansion can never scroll its own warning off the screen.
            let footer = usize::from(exp.view.omitted_lines > 0);
            let body = area.height.saturating_sub(1).saturating_sub(footer as u16);
            // No per-hunk `[a accept]` inside an expansion: a collapsed row is a single
            // accept (§6.3), so a hunk control there would promise something the reducer
            // will not do. The row's own `[A accept file]` is the only accept on screen,
            // and `[u restore]` goes with it — the row carries no hunks, so a hunk restore
            // there would ask about nothing (verifier (b) F5). `[m flag]` stays: `m` on an
            // expansion hunk quotes that hunk, and the row's flags read beside it.
            render_hunks(
                app,
                buf,
                Rect::new(area.x, area.y + 1, area.width, body),
                &exp.view.hunks,
                &row.flags,
                HunkControls::FlagOnly,
                hits,
            );
            if footer == 1 && area.height >= 2 {
                let text = format!(
                    "… {} lines omitted (cap {})",
                    with_thousands(exp.view.omitted_lines),
                    with_thousands(EXPAND_LINE_CAP)
                );
                buf.set_line(
                    area.x,
                    area.y + area.height - 1,
                    &single(text, dim()),
                    area.width,
                );
            }
            return;
        }
        _ => {}
    }
    render_hunks(
        app,
        buf,
        area,
        &row.hunks,
        &row.flags,
        HunkControls::All,
        hits,
    );
}

/// Which controls a hunk header carries — and, with them, which hit targets are registered
/// for it. A control that is not drawn is not clickable: the two are one decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HunkControls {
    /// A row's own hunks: `[a accept]`, `[u restore]`, `[m flag]`.
    All,
    /// A collapsed row's expansion: `[m flag]` only. Accepting or restoring one hunk of a
    /// row that carries none is not something the reducer will do (§6.3, and the
    /// expansion's line cap), so the labels that promise it are not drawn.
    FlagOnly,
}

/// Draw `hunks` into `area` from the app's diff cursor, with the header controls and the
/// selected-hunk band. The list is the row's own hunks, or a collapsed row's expansion
/// ([`App::view_hunks`] decides which the cursor is bounded by).
fn render_hunks(
    app: &App,
    buf: &mut Buffer,
    area: Rect,
    hunks: &[Hunk],
    flags: &[Flag],
    controls: HunkControls,
    hits: &mut HitMap,
) {
    if hunks.is_empty() || area.height == 0 {
        return;
    }
    let total = diff_lines(hunks);
    let scroll = app.diff.scroll.min(total.saturating_sub(1));
    let current = app.diff.hunk.min(hunks.len() - 1);
    // Deliverable 9: where a mouse press or drag turns into a diff line, and the inclusive
    // line range a live selection covers.
    hits.diff_body = Some(area);
    let selected = app.sel.map(|s| s.range());
    // Phase 13: **the** mapping from diff lines to screen rows, the same one the hit map
    // and the reducer's keep-visible arithmetic ask. The renderer no longer walks the
    // hunks itself, so there is no second answer to keep in step with this one.
    let rows = wrap::layout(hunks, scroll, area.width, area.height, app.wrap);
    hits.diff_rows = rows.iter().map(|r| r.line).collect();
    // The wrapped line being drawn, expanded once for all of its rows: a line can be a
    // megabyte, and expanding it again for each row was most of a frame (code verification
    // F2).
    let mut expanded: Option<(usize, String)> = None;
    for (y, row) in rows.iter().enumerate() {
        let y = y as u16;
        let (h, within) = (row.hunk, row.within);
        let hunk = &hunks[h];
        // The blank separator between two hunks: nothing to draw, it just spaces the
        // sections apart. It stays in the row table above, so a press on it answers the
        // line it always answered.
        let mut line = match &row.part {
            wrap::Part::Blank => continue,
            wrap::Part::Whole => hunk_line(hunk, within, h == current),
            wrap::Part::Slice { start, end, marker } => {
                let (tag, bytes) = &hunk.lines[within - 1];
                if expanded.as_ref().is_none_or(|(line, _)| *line != row.line) {
                    expanded = Some((row.line, line_text(bytes)));
                }
                let text = expanded.as_ref().map_or("", |(_, text)| text.as_str());
                hunk_row(*tag, text, *start, *end, marker.as_deref())
            }
        };
        let row_rect = Rect::new(area.x, area.y + y, area.width, 1);
        {
            if within == 0 {
                hits.targets.push((row_rect, Target::DiffHunk(h)));
                let style = if h == current {
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    dim()
                };
                let mut labels = Vec::with_capacity(3);
                let mut targets = Vec::with_capacity(3);
                if controls == HunkControls::All {
                    labels.push(format!("[{} accept]", control_key(app, "accept")));
                    targets.push(Target::HunkAccept(h));
                    labels.push(format!("[{} restore]", control_key(app, "restore")));
                    targets.push(Target::HunkRestore(h));
                }
                labels.push(format!("[{} flag]", control_key(app, "flag")));
                targets.push(Target::HunkFlag(h));
                let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                if let Some(note) = flag_note_for(flags, hunk) {
                    // The note reads beside the header it is about, so the reader sees what
                    // they already said here before they say it again — in the room left
                    // once the controls are reserved, because a flagged hunk is exactly the
                    // one whose `[m flag]` and `[u restore]` the reader still wants.
                    let budget = marker_budget(area.width, line.width(), &refs);
                    if let Some(text) = flag_marker(&note, budget) {
                        line.spans.push(Span::styled(text, style));
                    }
                }
                {
                    let at = right_align_run(&mut line, &refs, area.width, style);
                    for ((x, label), target) in at.into_iter().zip(&labels).zip(targets) {
                        if let Some(x) = x {
                            hits.targets.push((
                                Rect::new(area.x + x, area.y + y, label.width() as u16, 1),
                                target,
                            ));
                        }
                    }
                }
                if h == current {
                    band(&mut line, area.width, style);
                }
            }
            // Last, and over the hunk band: a selection is the reader's own mark, and it
            // reads as one run across the pane whatever is underneath it. Keyed on the
            // row's **line**, so a band covers every row of a line it covers.
            if selected.is_some_and(|(a, b)| row.line >= a && row.line <= b) {
                band(
                    &mut line,
                    area.width,
                    Style::new().add_modifier(Modifier::REVERSED),
                );
            }
            buf.set_line(area.x, area.y + y, &line, area.width);
        }
    }
}

/// The first line of the newest note flagged **on this hunk**, or `None`.
///
/// Matched on the header text the flag stored, not on the hunk's index: the index is where
/// the hunk was when it was flagged, and one edit above it moves every later hunk down. The
/// header carries the ranges, so it identifies the hunk within the file, and a flag whose
/// header no longer appears simply shows no marker rather than marking the wrong hunk.
fn flag_note_for(flags: &[Flag], hunk: &Hunk) -> Option<String> {
    let header = hunk_header(hunk);
    flags
        .iter()
        .rev()
        .find(|f| f.hunk.as_ref().is_some_and(|h| h.header == header))
        .map(|f| f.note.lines().next().unwrap_or("").to_owned())
}

/// Make `line` a full-width band: pad it out to `width` and put `style` under every span,
/// so the selected hunk's header reads as one run across the diff pane — the `[a accept]`
/// control visibly belonging to it — instead of two islands of inverse.
fn band(line: &mut Line<'static>, width: u16, style: Style) {
    let used = line.width();
    if used < width as usize {
        line.spans
            .push(Span::raw(" ".repeat(width as usize - used)));
    }
    for span in &mut line.spans {
        span.style = style.patch(span.style);
    }
}

/// The first key bound to `action`, as a control label (`a`, `A`); `?` when unbound.
fn control_key(app: &App, action: &str) -> String {
    app.keys_for(action)
        .first()
        .map(|s| key_label(s))
        .unwrap_or_else(|| "?".to_owned())
}

/// Append `control` right-aligned on `line` within `width` columns, at least two columns
/// after the text; returns its x offset, or `None` when it would not fit (then the line
/// is left as it was).
/// Several controls right-aligned as **one** run, space-separated, so a second call cannot
/// land on top of the first (each would right-align to the same edge). Returns one x offset
/// per label, in order.
///
/// A run that does not fit is retried without its last label, then without its last two, and
/// so on: a narrow pane loses the newest control rather than all of them, and `[a accept]`
/// — the oldest, and the one the hint line names first — is the last to go. The labels a
/// retry dropped come back `None`, and nothing is drawn for them (no hit target either, so
/// a click there falls through to the hunk header underneath).
fn right_align_run(
    line: &mut Line<'static>,
    labels: &[&str],
    width: u16,
    style: Style,
) -> Vec<Option<u16>> {
    for n in (1..=labels.len()).rev() {
        let run = labels[..n].join(" ");
        // `right_align` leaves the line untouched when it refuses, so each try is clean.
        if let Some(x) = right_align(line, &run, width, style) {
            let mut out = Vec::with_capacity(labels.len());
            let mut at = x;
            for label in &labels[..n] {
                out.push(Some(at));
                at += label.width() as u16 + 1;
            }
            out.resize(labels.len(), None);
            return out;
        }
    }
    vec![None; labels.len()]
}

fn right_align(line: &mut Line<'static>, control: &str, width: u16, style: Style) -> Option<u16> {
    let used = line.width();
    let need = used + 2 + control.width();
    if need > width as usize {
        return None;
    }
    let x = width as usize - control.width();
    line.spans.push(Span::raw(" ".repeat(x - used)));
    line.spans.push(Span::styled(control.to_owned(), style));
    Some(x as u16)
}

/// Line `i` of a hunk: 0 is the header (`@@ -a,b +c,d @@`, or `mode a → b` for a mode
/// change), then the hunk's lines with their `+`/`-`/space prefix.
fn hunk_line(hunk: &Hunk, i: usize, current: bool) -> Line<'static> {
    if i == 0 {
        let text = hunk_header(hunk);
        let style = if current {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().fg(Color::Cyan)
        };
        return Line::from(Span::styled(text, style));
    }
    let (tag, bytes) = &hunk.lines[i - 1];
    let text = line_text(bytes);
    match tag {
        Tag::Context => Line::from(format!(" {text}")),
        Tag::Insert => Line::from(Span::styled(format!("+{text}"), green())),
        Tag::Delete => Line::from(Span::styled(format!("-{text}"), red())),
    }
}

/// One **row** of a wrapped text line: the gutter mark, the slice of the line's text this
/// row carries, and the cap's marker when this is the last row of a line that was cut.
///
/// Only a line that takes more than one row comes here; a line that fits, and every line
/// with wrap off, is a [`wrap::Part::Whole`] and goes to [`hunk_line`] untouched. The gutter
/// and the colour are the line's own on every row, so a wrapped deletion never reads as
/// context half way down. On a continuation row the gutter mark alone is **dim**, so a
/// wrapped `+` line does not read as several added lines (design review F19, code
/// verification F5); the text keeps its full colour.
fn hunk_row(tag: Tag, text: &str, start: usize, end: usize, marker: Option<&str>) -> Line<'static> {
    let slice: String = text
        .chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect();
    let colour = match tag {
        Tag::Context => Style::new(),
        Tag::Insert => green(),
        Tag::Delete => red(),
    };
    let gutter = wrap::gutter_of(tag);
    let mut line = if start == 0 {
        // The first row is built the way `hunk_line` builds a whole line.
        Line::from(Span::styled(format!("{gutter}{slice}"), colour))
    } else {
        Line::from(vec![
            Span::styled(gutter.to_string(), colour.add_modifier(Modifier::DIM)),
            Span::styled(slice, colour),
        ])
    };
    if let Some(marker) = marker {
        // Dim, never the line's green or red (design review F15): a marker in the line's
        // own colour reads as content the agent wrote.
        line.spans.push(Span::styled(marker.to_owned(), dim()));
    }
    line
}

pub(super) fn line_text(bytes: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(bytes).into_owned();
    while s.ends_with('\n') || s.ends_with('\r') {
        s.pop();
    }
    s.replace('\t', "    ")
}

// ---- help --------------------------------------------------------------------------------

pub(super) fn key_label(spec: &str) -> String {
    match spec {
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "enter" => "⏎".into(),
        "esc" => "Esc".into(),
        "tab" => "Tab".into(),
        "pageup" => "PgUp".into(),
        "pagedown" => "PgDn".into(),
        "home" => "Home".into(),
        "end" => "End".into(),
        "space" => "Space".into(),
        s if s.starts_with("ctrl-") => format!("Ctrl-{}", s[5..].to_uppercase()),
        // `Alt-↑`, on the `Ctrl-` row's terms, so the arrow is the same glyph the `nav_up`
        // row shows. The key itself is what the reader is looking for; the modifier is a
        // prefix on it, not a different spelling of it.
        s if s.starts_with("alt-") => format!("Alt-{}", key_label(&s[4..])),
        // What a Mac sends for Option-z when the terminal does not make Option an Alt. The
        // reader is looking for the key they press, so the character is the footnote.
        "Ω" => "Opt-z (Ω)".into(),
        s => s.to_owned(),
    }
}

fn keys_label(specs: &[impl AsRef<str>]) -> String {
    specs
        .iter()
        .map(|s| key_label(s.as_ref()))
        .collect::<Vec<_>>()
        .join(" / ")
}

/// The keymap's rows, then the modal's fixed keys, then the mouse note.
/// Spaces between the two columns of the wide help overlay.
const HELP_GUTTER: usize = 3;

/// The key rows as the overlay's body: one column, or two when one does not fit.
///
/// The overlay has grown a keymap row at a time and it now overflows a 30-line terminal —
/// and it overflows *silently*, because the body is drawn with `take(inner.height)`: the
/// rows past the bottom are simply not there, and the help that is supposed to be the
/// answer to "what are the keys" stops naming half of them. Two columns are the cheapest
/// fix that keeps every row on screen.
///
/// The trigger is the overflow itself (`rows + 4 > area.height`, the same arithmetic the
/// caller's `height` clamps with) plus enough width for a second column. Order reads **down
/// the first column, then down the second** — the keymap's own order, so a reader looking
/// for a key finds it where the config file has it. A short terminal that is also narrow
/// gets one column and the old truncation; there is nothing better to do with 40 columns.
fn help_columns(keys: &[String], area: Rect) -> Vec<String> {
    // `+ 4` for what sits below the body — the blank, the newline note, `SELECT_NOTE` and
    // `TOUR_NOTE` — and `+ 4` for the border, the pad and the `any key closes` line: the
    // overlay's fixed overhead.
    if keys.len() + 4 + 4 <= area.height as usize {
        return keys.to_vec();
    }
    let split = keys.len().div_ceil(2);
    let (left, right) = keys.split_at(split);
    let width_of = |rows: &[String]| rows.iter().map(|r| r.width()).max().unwrap_or(0);
    // Each column is only as wide as its own rows need. Padding both to the widest row in
    // the whole table would cost the columns the very width the second one needs.
    let stride = width_of(left) + HELP_GUTTER;
    if stride + width_of(right) + 4 > area.width as usize {
        // Two columns would have to be truncated to fit, which is the failure this is
        // fixing. One column and the old vertical clipping is no worse.
        return keys.to_vec();
    }
    left.iter()
        .enumerate()
        .map(|(i, l)| match right.get(i) {
            Some(r) => {
                let mut line = l.clone();
                line.push_str(&" ".repeat(stride - l.width()));
                line.push_str(r);
                line
            }
            // An odd count leaves the last left-column row alone rather than padding it to
            // a column width nothing sits beside.
            None => l.clone(),
        })
        .collect()
}

/// The frame width at which the two-column overlay holds this keymap whole — the figure
/// the clip notice quotes (Design pass D12, ruling R12). It mirrors [`help_columns`]'s own
/// test rather than restating a number, so it follows the keymap instead of going stale.
fn help_two_column_width(keys: &[String]) -> usize {
    let split = keys.len().div_ceil(2);
    let (left, right) = keys.split_at(split);
    let width_of = |rows: &[String]| rows.iter().map(|r| r.width()).max().unwrap_or(0);
    width_of(left) + HELP_GUTTER + width_of(right) + 4
}

fn render_help(app: &App, buf: &mut Buffer, area: Rect) {
    let named: Vec<(&str, String)> = app
        .keymap
        .iter()
        .map(|(name, specs)| (name.as_str(), keys_label(specs)))
        .chain(
            MODAL_KEYS
                .iter()
                .map(|(name, specs)| (*name, keys_label(specs))),
        )
        .map(|(name, keys)| (name, format!("{keys:<14} {}", Action::describe(name))))
        .collect();
    let keys: Vec<String> = named.iter().map(|(_, row)| row.clone()).collect();
    let body = help_columns(&keys, area);
    let mut rows = body.clone();
    rows.push(String::new());
    rows.push(newline_note(app.enhanced).to_owned());
    rows.push(SELECT_NOTE.to_owned());
    rows.push(TOUR_NOTE.to_owned());
    let width = (rows.iter().map(|r| r.width()).max().unwrap_or(0) + 4).min(area.width as usize);
    let height = (rows.len() + 4).min(area.height as usize);
    let rect = Rect::new(
        area.x + (area.width - width as u16) / 2,
        area.y + (area.height - height as u16) / 2,
        width as u16,
        height as u16,
    );
    Clear.render(rect, buf);
    let block = Block::bordered()
        .title(" keys ")
        .border_style(focused_border());
    let inner = block.inner(rect);
    block.render(rect, buf);
    // The last inner row belongs to `any key closes` at every height, so `cap` is the
    // body's room: one row short of the box's. `help_columns`'s `+ 4` and the `height`
    // arithmetic above both reserve that row already, but the draw wrote the line only
    // where the body fell short of the box — so at the one height where the body fits
    // exactly, the body took the row and the way out of the overlay went unsaid
    // (verifier (b) F5). The clip path's own arithmetic is unchanged: it reserved the same
    // row by counting four instead of three.
    let cap = (inner.height as usize).saturating_sub(1);
    let mut clip_notice = None;
    if rows.len() > cap {
        // Too narrow for two columns *and* too short for one (80×30 with this keymap): the
        // overlay clips, and what it clips is key rows — never the footer. A reader who
        // cannot see every key can still see what the mouse does and how to leave.
        //
        // Three rows are reserved out of `cap` here, the newline note, `SELECT_NOTE` and
        // `TOUR_NOTE`; the `any key closes` line below them is already out of `cap` by
        // construction.
        // Reserving too few put the body's last row on the footer's row, so the footer was
        // the thing the clip dropped (verifier (b) F4).
        //
        // The blank separator is *not* reserved — it is the first thing the clip spends.
        //
        // Design pass D12 / ruling R12: a fourth row goes to **saying so**. The overlay
        // clipped in silence, which is the failure two columns were meant to end; the last
        // body row is now a dim `… N more keys (100 columns shows all)`, so a reader who
        // cannot see a key knows there are more of them and what to do about it.
        let quit = named
            .iter()
            .find(|(name, _)| *name == "quit")
            .map(|(_, row)| row.clone());
        let mut keep = cap.saturating_sub(4);
        // …and `quit` is pinned to the end of what survives, when what survives does not
        // already carry it. The keymap grows — Phase 8 alone adds four rows — and a clip
        // that simply takes the first N pushes the last row off first, which in this keymap
        // is the one row a reader who opened the overlay by accident most needs.
        // `any key closes` gets them out of the overlay; this gets them out of lastcall.
        let pin_quit = quit.as_ref().is_some_and(|q| {
            !body[..keep.min(body.len())]
                .iter()
                .any(|r| r.contains(q.as_str()))
        });
        if pin_quit {
            keep = keep.saturating_sub(1);
        }
        let keep = keep.min(body.len());
        // The count is of **keys**, not rows: in two columns a body row carries two of
        // them, and counting rows said `2 more keys` over four hidden ones at 100×20
        // (verifier (b) F1). `help_columns` splits the table at `keys.len().div_ceil(2)`,
        // so the rows past the right column's length carry one key each.
        let keys_in_row = |i: usize| -> usize {
            let right_len = keys.len() - keys.len().div_ceil(2);
            if body.len() < keys.len() && i < right_len {
                2
            } else {
                1
            }
        };
        let shown: usize = (0..keep).map(keys_in_row).sum::<usize>() + usize::from(pin_quit);
        let hidden = keys.len().saturating_sub(shown);
        // The figure is the two-column width the keymap needs, computed rather than
        // written down. Where the frame is already that wide the clip is the *height*, and
        // saying "100 columns shows all" at 120 columns would be a lie — so that case names
        // the other dimension instead.
        let remedy = if area.width as usize >= help_two_column_width(&keys) {
            "a taller window shows all".to_owned()
        } else {
            format!("{} columns shows all", help_two_column_width(&keys))
        };
        rows = body[..keep].to_vec();
        clip_notice = Some(rows.len());
        rows.push(format!("… {} ({remedy})", plural(hidden, "more key")));
        if let Some(q) = quit.filter(|_| pin_quit) {
            rows.push(q);
        }
        rows.push(newline_note(app.enhanced).to_owned());
        rows.push(SELECT_NOTE.to_owned());
        rows.push(TOUR_NOTE.to_owned());
        rows.truncate(cap);
    }
    for (i, row) in rows.iter().take(cap).enumerate() {
        buf.set_stringn(
            inner.x + 1,
            inner.y + i as u16,
            row,
            inner.width.saturating_sub(1) as usize,
            // The clip notice is dim: it is the overlay talking about itself, not a key.
            if clip_notice == Some(i) {
                dim()
            } else {
                Style::new()
            },
        );
    }
    buf.set_stringn(
        inner.x + 1,
        inner.y + inner.height.saturating_sub(1),
        "any key closes",
        inner.width.saturating_sub(1) as usize,
        dim(),
    );
}

/// The confirm modal (§6.7), centered like the help overlay. One box, three operations: the
/// title is ` accept `, ` restore ` or ` review `, and the first row comes from the scope.
///
/// An accept's numbers come from `App::confirm_counts`, i.e. the held piles as they are at
/// this frame: `Accept all <N> files in <root>?` (one root) or `across <R> repos?`, then
/// `<g> grouped upstream · <c> collapsed` only when either is non-zero. A restore covers one
/// row, so it has one question row and nothing to tally (F11). So does the post-`$EDITOR`
/// blessing (Phase 8 deliverable 3), whose question names the path and nothing else. It is
/// framed as *intent* ("edited — mark every hunk reviewed?"), never as detection: lastcall
/// only knows the bytes differ from when the editor opened, not who wrote them, and the
/// sponsor's Gate 8 run read the earlier "changed while your editor was open" as a claim that
/// someone else had. No hunk count either — the row on screen may predate the save.
fn render_confirm(app: &App, buf: &mut Buffer, area: Rect) {
    if let Some(path) = app.confirm_bless() {
        let question = format!(
            "{} edited — mark every hunk in it reviewed?",
            String::from_utf8_lossy(path)
        );
        return confirm_box(" review ", vec![question], buf, area);
    }
    if let Some(path) = app.confirm_discard() {
        let question = format!("Discard changes to {}?", String::from_utf8_lossy(path));
        return confirm_box(" discard ", vec![question], buf, area);
    }
    let (title, rows) = match app.confirm_restore() {
        Some(scope) => (" restore ", vec![restore_question(scope)]),
        None => {
            let Some(counts) = app.confirm_counts() else {
                return;
            };
            let target = match counts.roots.as_slice() {
                [one] => format!("in {one}"),
                many => format!("across {}", plural(many.len(), "repo")),
            };
            let mut rows = vec![format!(
                "Accept all {} {target}?",
                plural(counts.files, "file")
            )];
            if counts.grouped > 0 || counts.collapsed > 0 {
                rows.push(format!(
                    "{} grouped upstream · {} collapsed",
                    counts.grouped, counts.collapsed
                ));
            }
            (" accept ", rows)
        }
    };
    confirm_box(title, rows, buf, area);
}

/// The box every confirm shares: the question rows, a blank line, then the modal keys.
fn confirm_box(title: &str, mut rows: Vec<String>, buf: &mut Buffer, area: Rect) {
    rows.push(String::new());
    rows.push(
        MODAL_KEYS
            .iter()
            .map(|(name, specs)| format!("{} {}", keys_label(specs), Action::describe(name)))
            .collect::<Vec<_>>()
            .join("    "),
    );
    let width = (rows.iter().map(|r| r.width()).max().unwrap_or(0) + 4).min(area.width as usize);
    let height = (rows.len() + 2).min(area.height as usize);
    let rect = Rect::new(
        area.x + (area.width - width as u16) / 2,
        area.y + (area.height - height as u16) / 2,
        width as u16,
        height as u16,
    );
    Clear.render(rect, buf);
    let block = Block::bordered()
        .title(title)
        .border_style(focused_border());
    let inner = block.inner(rect);
    block.render(rect, buf);
    for (i, row) in rows.iter().take(inner.height as usize).enumerate() {
        let style = if i == 0 { bold() } else { Style::new() };
        buf.set_stringn(
            inner.x + 1,
            inner.y + i as u16,
            row,
            inner.width.saturating_sub(1) as usize,
            style,
        );
    }
}

/// The note modal: what is being flagged, the note being typed, and the keys that end it.
///
/// The **title names the target** — ` flag hunk 2 of 3 ` or ` flag whole file ` — so the
/// frame of the box answers "what am I flagging?" even when a long path has been ellipsized
/// on the line below it (ruling P4).
///
/// The text area is a fixed [`NOTE_ROWS`] lines high whatever is typed, so the box does not
/// jump under the reader's hands as the note grows; past that it scrolls to keep the caret
/// (`▌`) in view. The caret is drawn into the text rather than set on the terminal so that
/// one `App` renders to one buffer — the snapshot tier can see where the cursor is.
///
/// The scroll comes from the note's own [`TextBuf`](super::textbuf::TextBuf) viewport, run
/// on a **copy**: `render` is a function of `&App` and may not move the buffer's `top`. The
/// copy always starts at the top, so the window still ends at the caret's row — the same
/// rule the modal has had since Phase 7, now with the buffer's soft wrapping under it.
fn render_note(app: &App, buf: &mut Buffer, area: Rect) {
    let Some(note) = &app.note else {
        return;
    };
    let width = NOTE_WIDTH.min(area.width.saturating_sub(4)).max(8);
    // border + target + blank + text + blank + keys
    let height = (NOTE_ROWS + 6).min(area.height);
    let rect = centered(area, width, height);
    // Bold, as the kickoff's deliverable 5 asks: the title is the answer to "what am I
    // flagging?", and it is the one line of the box a reviewer must not skim past.
    let inner = modal_block(Line::styled(note.target.modal_title(), bold()), rect, buf);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let text_width = inner.width.saturating_sub(2) as usize;
    let mut rows: Vec<(String, Style)> = vec![
        (ellipsize(&note.target.label(), text_width), bold()),
        (String::new(), Style::new()),
    ];
    let view = note
        .buf
        .clone()
        .viewport(NOTE_ROWS as usize, text_width.max(1), Wrap::Soft);
    for i in 0..NOTE_ROWS as usize {
        let line = match view.rows.get(i) {
            Some(text) if i == view.caret.0 => with_caret(text, view.caret.1),
            Some(text) => text.clone(),
            None => String::new(),
        };
        rows.push((line, Style::new()));
    }
    rows.push((String::new(), Style::new()));
    rows.push((note_keys(app.enhanced).to_owned(), dim()));
    for (i, (row, style)) in rows.iter().take(inner.height as usize).enumerate() {
        buf.set_stringn(
            inner.x + 1,
            inner.y + i as u16,
            row,
            inner.width.saturating_sub(1) as usize,
            *style,
        );
    }
}

/// The snooze modal: which repository, for how many days, and the keys that end it
/// (Amendment v1.11).
///
/// The note modal's shape with a number in place of the text area — one line, because that
/// is the whole question. The field carries the same caret the note does, so a reader who
/// has used `m` knows where their digits will land, and an emptied field shows the caret
/// alone rather than a `0` nobody typed.
fn render_snooze(app: &App, buf: &mut Buffer, area: Rect) {
    let Some(entry) = &app.snooze else {
        return;
    };
    let rows: Vec<(String, Style)> = vec![
        (
            format!(
                "snooze {} for [{}{NOTE_CARET}] day(s)",
                entry.name, entry.days
            ),
            bold(),
        ),
        (String::new(), Style::new()),
        (SNOOZE_KEYS.to_owned(), dim()),
    ];
    let width = (rows.iter().map(|(r, _)| r.width()).max().unwrap_or(0) + 4)
        .min(area.width as usize)
        .max(8);
    let height = (rows.len() + 2).min(area.height as usize);
    let rect = centered(area, width as u16, height as u16);
    let inner = modal_block(" snooze ", rect, buf);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    for (i, (row, style)) in rows.iter().take(inner.height as usize).enumerate() {
        buf.set_stringn(
            inner.x + 1,
            inner.y + i as u16,
            row,
            inner.width.saturating_sub(1) as usize,
            *style,
        );
    }
}

/// The first-launch welcome overlay (Amendment v1.11, deliverable 1).
///
/// The confirm modal's shape at the size of a card: a centred box, at least
/// [`tour::MIN_COLS`](super::tour::MIN_COLS) wide, the body wrapped to it. The card's text
/// is `tour.rs`'s — what a card says and how its columns line up is the card's business —
/// and what happens here is the painting and the hit map, because both are facts about a
/// frame rather than about the tour.
///
/// The height ladder spends the blank separator lines first: a card on a short terminal
/// closes up rather than losing its footer, which is the one line saying how to leave. A
/// card that would rather give up a paragraph than its spacing says so itself: the height
/// budget goes to `Tour::lines` before the painter sees a row, which is how the depth card
/// drops its hint block at 60x14 (deliverable 8).
fn render_tour(app: &App, buf: &mut Buffer, area: Rect, hits: &mut HitMap) {
    let Some(tour) = &app.tour else {
        return;
    };
    // Two borders and one column of padding on each side, and a column of the screen left
    // showing on each side of the box so it reads as an overlay rather than a repaint.
    let max = area.width.saturating_sub(6) as usize;
    let floor = (TOUR_MIN_COLS as usize).saturating_sub(4).min(max);
    let text = tour.natural(app).clamp(floor, max.max(1));
    let mut rows = tour.lines(app, text, area.height.saturating_sub(2) as usize);
    let width = (rows
        .iter()
        .map(|l| l.text.width())
        .max()
        .unwrap_or(0)
        .max(floor)
        + 4)
    .min(area.width as usize);
    let mut height = rows.len() + 2;
    while height > area.height as usize {
        let Some(blank) = rows.iter().position(|l| l.text.is_empty()) else {
            break;
        };
        rows.remove(blank);
        height -= 1;
    }
    let rect = centered(area, width as u16, height.min(area.height as usize) as u16);
    let inner = modal_block(" welcome ", rect, buf);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    // A failed write leaves a choice card with no choice left to make — the setting is
    // already applied — so its footer is what the mouse advances from.
    let footer_advances = !tour.card().is_choice() || tour.failed.is_some();
    for (i, row) in rows.iter().take(inner.height as usize).enumerate() {
        let y = inner.y + i as u16;
        let style = match row.kind {
            TourLine::Title => bold(),
            TourLine::Choice(n) if n == tour.row => bold(),
            TourLine::Choice(_) => Style::new(),
            TourLine::Footer => dim(),
            TourLine::Body => Style::new(),
        };
        buf.set_stringn(
            inner.x + 1,
            y,
            &row.text,
            inner.width.saturating_sub(1) as usize,
            style,
        );
        let target = match row.kind {
            TourLine::Choice(n) => Some(Target::TourRow(n)),
            TourLine::Footer if footer_advances => Some(Target::TourRow(0)),
            _ => None,
        };
        if let Some(target) = target {
            hits.targets
                .push((Rect::new(inner.x, y, inner.width, 1), target));
        }
    }
}

/// The agent picker: which pane the export goes to. The flag is already on disk when this
/// opens, so `Esc` costs only the send — which the first row says out loud.
fn render_picker(app: &App, buf: &mut Buffer, area: Rect) {
    let Some(picker) = &app.picker else {
        return;
    };
    let mut rows: Vec<(String, Style)> = vec![
        (format!("flagged {} — send to:", picker.label), bold()),
        (String::new(), Style::new()),
    ];
    for (i, c) in picker.candidates.iter().enumerate() {
        let mark = if i == picker.selected { "▸ " } else { "  " };
        rows.push((
            format!(
                "{mark}{} · {} · {}",
                c.label,
                c.workspace_label,
                c.status.as_str()
            ),
            if i == picker.selected {
                bold()
            } else {
                Style::new()
            },
        ));
    }
    rows.push((String::new(), Style::new()));
    rows.push((PICK_KEYS.to_owned(), dim()));
    let width =
        (rows.iter().map(|(r, _)| r.width()).max().unwrap_or(0) + 4).min(area.width as usize);
    let height = (rows.len() + 2).min(area.height as usize);
    let rect = centered(area, width as u16, height as u16);
    let inner = modal_block(" send to ", rect, buf);
    for (i, (row, style)) in rows.iter().take(inner.height as usize).enumerate() {
        buf.set_stringn(
            inner.x + 1,
            inner.y + i as u16,
            row,
            inner.width.saturating_sub(1) as usize,
            *style,
        );
    }
}

/// `row` with [`NOTE_CARET`] drawn at display column `col`, padded when the caret sits past
/// the end of the line (which is where it sits most of the time — one column after the last
/// character typed).
fn with_caret(row: &str, col: usize) -> String {
    let mut out = String::new();
    let mut at = 0usize;
    let mut chars = row.chars();
    for c in chars.by_ref() {
        if at >= col {
            out.push_str(NOTE_CARET);
            out.push(c);
            out.extend(chars);
            return out;
        }
        at += c.width().unwrap_or(0);
        out.push(c);
    }
    // Past the end: pad to the column, then the caret.
    for _ in at..col {
        out.push(' ');
    }
    out.push_str(NOTE_CARET);
    out
}

/// A `width`×`height` rect centred in `area`, clamped to it.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// The copy cue: one centred, `Clear`-backed line over the diff pane (deliverable 9). It is
/// deliberately not the status line — the status is the record of what the *engine* did,
/// and a copy must not overwrite an accept's or a refusal's sentence.
fn render_cue(text: &str, buf: &mut Buffer, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let rect = centered(area, text.width() as u16 + 2, 1);
    Clear.render(rect, buf);
    buf.set_line(
        rect.x,
        rect.y,
        &Line::from(Span::styled(
            format!(" {text} "),
            Style::new().add_modifier(Modifier::REVERSED),
        )),
        rect.width,
    );
}

/// Clear `rect`, draw the focused border with `title`, and hand back the inside.
fn modal_block<'a>(title: impl Into<Line<'a>>, rect: Rect, buf: &mut Buffer) -> Rect {
    Clear.render(rect, buf);
    let block = Block::bordered()
        .title(title.into())
        .border_style(focused_border());
    let inner = block.inner(rect);
    block.render(rect, buf);
    inner
}

// ---- helpers -----------------------------------------------------------------------------

/// Truncate `s` to at most `max` columns (unicode width), ending in `…` when cut.
pub fn ellipsize(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Truncate `s` to at most `max` columns from the **head**, cutting at a `/` so what is
/// left still reads as a path: `…lastcall/src/tui/render.rs` (Design pass D9, ruling R10).
///
/// The `/` chosen is the leftmost one whose remainder fits, so the longest path tail that
/// fits is what is shown. When not even `…<basename>` fits, the **basename itself gives
/// from the head** (`…_for_a_file.rs`), so the extension survives and — in the editor
/// header — the position and `· unsaved` do too: a basename floor let the row's tail clip
/// take exactly the parts R10 said were the wrong ones to lose (verifier (b) F2). A string
/// with no `/` at all is head-cut the same way.
pub fn ellipsize_head(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_owned();
    }
    let mut basename = s;
    for (i, c) in s.char_indices() {
        if c != '/' {
            continue;
        }
        let tail = &s[i + 1..];
        basename = tail;
        if tail.width() < max {
            return format!("…{tail}");
        }
    }
    head_cut(basename, max)
}

/// `…` plus the longest tail of `s` that fits in `max` columns; empty at `max == 0`.
fn head_cut(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut kept = String::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > max - 1 {
            break;
        }
        kept.insert(0, c);
        used += w;
    }
    format!("…{kept}")
}

/// One line per run of non-default cells: `y x0..x1 fg bg modifiers` (`x1` exclusive,
/// modifiers `|`-joined or `-`). `TestBackend`'s `Display` shows symbols only, so
/// snapshots pair it with this.
pub fn styles(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in area.y..area.bottom() {
        let mut run: Option<(u16, Color, Color, Modifier)> = None;
        let flush = |run: &mut Option<(u16, Color, Color, Modifier)>, x1: u16, out: &mut String| {
            if let Some((x0, fg, bg, m)) = run.take() {
                out.push_str(&format!(
                    "{y} {x0}..{x1} {fg:?} {bg:?} {}\n",
                    modifier_names(m)
                ));
            }
        };
        for x in area.x..area.right() {
            let cell = &buf[(x, y)];
            let key = (cell.fg, cell.bg, cell.modifier);
            let default = key == (Color::Reset, Color::Reset, Modifier::empty());
            match run {
                Some((_, fg, bg, m)) if !default && (fg, bg, m) == key => {}
                _ => {
                    flush(&mut run, x, &mut out);
                    if !default {
                        run = Some((x, key.0, key.1, key.2));
                    }
                }
            }
        }
        flush(&mut run, area.right(), &mut out);
    }
    out
}

fn modifier_names(m: Modifier) -> String {
    const NAMES: &[(Modifier, &str)] = &[
        (Modifier::BOLD, "BOLD"),
        (Modifier::DIM, "DIM"),
        (Modifier::ITALIC, "ITALIC"),
        (Modifier::UNDERLINED, "UNDERLINED"),
        (Modifier::SLOW_BLINK, "SLOW_BLINK"),
        (Modifier::RAPID_BLINK, "RAPID_BLINK"),
        (Modifier::REVERSED, "REVERSED"),
        (Modifier::HIDDEN, "HIDDEN"),
        (Modifier::CROSSED_OUT, "CROSSED_OUT"),
    ];
    let names: Vec<&str> = NAMES
        .iter()
        .filter(|(f, _)| m.contains(*f))
        .map(|(_, n)| *n)
        .collect();
    if names.is_empty() {
        "-".to_owned()
    } else {
        names.join("|")
    }
}

#[cfg(test)]
mod tests {
    use super::super::app::{Changed, diff_len, testfix::*};
    use super::super::input::SnoozeKey;
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn frame_of(app: &App, w: u16, h: u16) -> (String, String) {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| {
                render(app, f);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (terminal.backend().to_string(), styles(&buf))
    }

    /// One root with `n` rows `p00`..`pNN`, so the nav's lines are exactly the root name
    /// (line 0), its branch line (line 1) and one line per row from line 2.
    fn one_root(n: usize) -> App {
        let mut app = App::new();
        app.sync_roots(vec![meta("alpha")]);
        app.apply(pile_event("alpha", rows_n(n, 0, 0)));
        app.handle(Action::Resize(100, 30));
        app
    }

    /// Draw just the nav into a `rows`-high pane and report the offset it used and where
    /// each drawn line landed.
    fn nav_only(app: &mut App, rows: u16) -> HitMap {
        let area = Rect::new(0, 0, 40, rows);
        let mut buf = Buffer::empty(area);
        let mut hits = HitMap::default();
        render_nav(app, &mut buf, area, &mut hits);
        if let Some(top) = hits.nav_top {
            app.nav_top = top;
        }
        hits
    }

    /// Deliverable 9: the nav offset persists between frames and moves the **minimum** that
    /// brings the selection back on screen. Before, it was recomputed from the selection
    /// every frame, so a nav longer than the pane snapped back to the top the moment the
    /// selection was visible.
    #[test]
    fn render_nav_offset_scrolls_the_minimum_to_reach_the_selection() {
        // 38 rows -> 40 nav lines: the root, its branch line, then one per row.
        let mut app = one_root(38);
        assert_eq!(app.nav_top, 0);

        // Line 25 (row p23) in a 20-row pane: the bottom of the window lands on it.
        app.select(Some(row("alpha", "p23")));
        assert_eq!(nav_only(&mut app, 20).nav_top, Some(6), "25 + 1 − 20");

        // Line 20 is inside [6, 26): a selection already on screen scrolls nothing.
        app.select(Some(row("alpha", "p18")));
        assert_eq!(nav_only(&mut app, 20).nav_top, Some(6));

        // Line 5 is above the window: it becomes the top, not the bottom.
        app.select(Some(row("alpha", "p03")));
        assert_eq!(nav_only(&mut app, 20).nav_top, Some(5));

        // The last line can never leave the pane less than full.
        app.select(Some(row("alpha", "p37")));
        assert_eq!(nav_only(&mut app, 20).nav_top, Some(20), "40 − 20");
    }

    /// A click selects the line under the pointer and the view does not jump: the reader
    /// clicked what they could see, so there is nothing to scroll to.
    #[test]
    fn render_nav_offset_is_unchanged_by_a_click_on_a_visible_row() {
        let mut app = one_root(38);
        app.select(Some(row("alpha", "p23")));
        let hits = nav_only(&mut app, 20);
        assert_eq!(app.nav_top, 6);

        // The pane's third screen row is nav line 8 — row p06.
        let target = hits.at(0, 2).expect("a nav target").clone();
        assert_eq!(target, Target::NavRow(root("alpha"), b"p06".to_vec()));
        app.hit(target);
        assert_eq!(app.selection, Some(row("alpha", "p06")));
        assert_eq!(
            nav_only(&mut app, 20).nav_top,
            Some(6),
            "the clicked line was already on screen"
        );
    }

    /// A pile that empties most of a root leaves an offset past the end of the list; the
    /// clamp pulls it back so the pane is full rather than blank.
    #[test]
    fn render_nav_offset_clamps_when_the_list_shrinks() {
        let mut app = one_root(38);
        app.select(Some(row("alpha", "p37")));
        assert_eq!(nav_only(&mut app, 20).nav_top, Some(20));

        // 8 rows -> 10 lines, which is shorter than the pane: the only valid offset is 0.
        app.apply(pile_event_seq("alpha", 2, rows_n(8, 0, 0)));
        assert_eq!(
            app.nav_top, 20,
            "the reducers leave the offset alone; the clamp is render's job"
        );
        assert_eq!(nav_only(&mut app, 20).nav_top, Some(0));

        // 30 rows -> 32 lines: the deepest a 20-row pane can start is line 12.
        app.apply(pile_event_seq("alpha", 3, rows_n(30, 0, 0)));
        app.nav_top = 25;
        app.selection = None;
        assert_eq!(nav_only(&mut app, 20).nav_top, Some(12));
    }

    /// Phase 6 deliverable 4: the collapsed row's header offers `[e expand]`, the answer
    /// replaces the empty pane with hunks, and a truncated answer says so on the last line
    /// — where scrolling can never push the warning off the screen.
    #[test]
    fn render_collapsed_row_offers_expand_and_shows_the_cap_footer() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.apply(pile_event_seq(
            "alpha",
            1,
            alpha_collapsed(lastcall_engine::scan::Collapsed::Glob),
        ));
        app.select(Some(row("alpha", "f1")));
        let (before, _) = frame_of(&app, 100, 30);
        assert!(before.contains("collapsed (glob)"), "{before}");
        assert!(before.contains("[e expand]"), "{before}");
        assert!(
            !before.contains("@@ -"),
            "nothing is expanded yet: {before}"
        );

        let asked = app.selected_row().unwrap().clone();
        app.set_expanded(root("alpha"), &asked, expansion_of(2, 1_234));
        let (after, _) = frame_of(&app, 100, 30);
        assert!(
            after.contains("collapsed (glob)"),
            "the header stays: {after}"
        );
        assert!(after.contains("@@ -"), "the hunks are on screen: {after}");
        assert!(
            after.contains("… 1,234 lines omitted (cap 2,000)"),
            "{after}"
        );

        // A whole answer has no footer.
        app.set_expanded(root("alpha"), &asked, expansion_of(2, 0));
        let (whole, _) = frame_of(&app, 100, 30);
        assert!(!whole.contains("lines omitted"), "{whole}");
    }

    /// A collapsed row whose modes differ names the change in its header: a mode-only
    /// change has no content hunks, so `+0 −0` alone would read as "nothing happened".
    #[test]
    fn render_collapsed_header_names_a_mode_change() {
        use lastcall_engine::git::Mode;
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        let mut pile = alpha_collapsed(lastcall_engine::scan::Collapsed::Glob);
        pile.rows[0].current.as_mut().unwrap().mode = Mode::Executable;
        app.apply(pile_event_seq("alpha", 1, pile));
        app.select(Some(row("alpha", "f1")));
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(frame.contains("mode 100644 → 100755"), "{frame}");
    }

    /// A binary row says why there is nothing to expand and draws no control.
    #[test]
    fn render_binary_row_offers_no_expand_control() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.apply(pile_event_seq(
            "alpha",
            1,
            alpha_collapsed(lastcall_engine::scan::Collapsed::Binary),
        ));
        app.select(Some(row("alpha", "f1")));
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(
            frame.contains("collapsed (binary)") && frame.contains("not expandable"),
            "{frame}"
        );
        assert!(!frame.contains("expand]"), "{frame}");
    }

    #[test]
    fn render_ellipsize_counts_columns_not_chars() {
        assert_eq!(ellipsize("abc", 3), "abc");
        assert_eq!(ellipsize("abcd", 3), "ab…");
        assert_eq!(ellipsize("日本語.md", 5), "日本…");
        assert_eq!(ellipsize("日本語", 6), "日本語");
        assert_eq!(ellipsize("abc", 0), "");
        assert_eq!(ellipsize("abc", 1), "…");
    }

    /// Design pass D9 (ruling R10): the editor header's path gives from the head and cuts
    /// at a `/`, so what is left still reads as a path — and inside the basename, from the
    /// head, when not even that fits (verifier (b) F2).
    #[test]
    fn render_ellipsize_head_cuts_at_a_slash_then_inside_the_basename() {
        let p = "home/aaron/dev/git/lastcall/src/tui/render.rs";
        assert_eq!(ellipsize_head(p, 60), p, "it fits: untouched");
        assert_eq!(ellipsize_head(p, p.len()), p, "exactly: untouched");
        // The leftmost `/` whose remainder fits, so the longest path tail is what shows —
        // and every cut lands on a segment boundary, never mid-segment.
        assert_eq!(
            ellipsize_head(p, 43),
            "…aaron/dev/git/lastcall/src/tui/render.rs",
            "one column short of the whole drops the first segment, not one character"
        );
        assert_eq!(
            ellipsize_head(p, 41),
            "…aaron/dev/git/lastcall/src/tui/render.rs"
        );
        assert_eq!(ellipsize_head(p, 40), "…dev/git/lastcall/src/tui/render.rs");
        assert_eq!(ellipsize_head(p, 35), "…dev/git/lastcall/src/tui/render.rs");
        assert_eq!(ellipsize_head(p, 34), "…git/lastcall/src/tui/render.rs");
        assert_eq!(ellipsize_head(p, 31), "…git/lastcall/src/tui/render.rs");
        assert_eq!(ellipsize_head(p, 30), "…lastcall/src/tui/render.rs");
        assert_eq!(ellipsize_head(p, 20), "…src/tui/render.rs");
        assert_eq!(ellipsize_head(p, 14), "…tui/render.rs");
        assert_eq!(ellipsize_head(p, 13), "…render.rs");
        assert_eq!(ellipsize_head(p, 10), "…render.rs");
        // Below `…<basename>` the basename itself gives from the head, so the extension
        // survives and nothing to the right of the path has to (verifier (b) F2).
        assert_eq!(ellipsize_head(p, 9), "…ender.rs");
        assert_eq!(ellipsize_head(p, 4), "….rs");
        assert_eq!(ellipsize_head(p, 1), "…");
        assert_eq!(ellipsize_head(p, 0), "");
        // No `/`: still from the head.
        assert_eq!(ellipsize_head("render.rs", 5), "…r.rs");
        // Columns, not bytes.
        assert_eq!(ellipsize_head("a/日本語.md", 10), "…日本語.md");
        assert_eq!(ellipsize_head("a/日本語.md", 8), "…本語.md");
        assert_eq!(ellipsize_head("a/日本語.md", 6), "…語.md");
    }

    /// The other half of D9: `· unsaved` is reserved at every width, so the header does
    /// not shift under the reader on the first keystroke, and the position never gives.
    #[test]
    fn render_editor_header_keeps_the_position_and_reserves_unsaved() {
        let mut app = three_roots();
        app.handle(Action::Resize(60, 20));
        let mut pile = pile("alpha");
        pile.rows[0].path = b"crates/lastcall/src/tui/render.rs".to_vec();
        app.apply(pile_event("alpha", pile));
        app.select(Some(Selection::Row(
            root("alpha"),
            b"crates/lastcall/src/tui/render.rs".to_vec(),
        )));
        app.handle(Action::Open);
        let open = match app.handle(Action::Edit).1 {
            Some(crate::tui::app::Effect::EditInline(open)) => open,
            other => panic!("an inline-edit effect, got {other:?}"),
        };
        app.edit_read(open, Ok(b"one\ntwo\nthree\n".to_vec()));
        let clean = frame_of(&app, 60, 20).0;
        let header = clean.lines().next().expect("a header").to_owned();
        assert!(
            header.contains("…lastcall/src/tui/render.rs · line 1/"),
            "the head gives, the position never does: {header}"
        );
        assert!(!header.contains("unsaved"), "not dirty yet: {header}");

        app.handle(Action::Editor(crate::tui::input::EditorKey::Edit(
            crate::tui::input::EditKey::Insert("x".to_owned()),
        )));
        let dirty = frame_of(&app, 60, 20).0;
        let dirty_header = dirty.lines().next().expect("a header").to_owned();
        assert!(dirty_header.contains(" · unsaved"), "{dirty_header}");
        assert!(
            dirty_header.starts_with(header.split(" · line").next().expect("a path")),
            "the path did not move when `· unsaved` appeared:\n{header}\n{dirty_header}"
        );

        // Verifier (b) F2: a basename wider than the budget used to overflow into the
        // row's tail clip, which took `unsaved` and then the position — at 60 and 80
        // columns with this name, and at `MIN_SIZE` for a four-digit line count. The
        // basename gives instead.
        let long = b"src/a_basename_that_is_really_quite_long_indeed_for_a_file.rs".to_vec();
        for (w, path) in [
            (60u16, long.clone()),
            (80, long.clone()),
            (40, b"src/tui/render.rs".to_vec()),
        ] {
            let mut app = three_roots();
            app.handle(Action::Resize(w, 20));
            let mut wide = self::pile("alpha");
            wide.rows[0].path = path.clone();
            app.apply(pile_event("alpha", wide));
            app.select(Some(Selection::Row(root("alpha"), path.clone())));
            app.handle(Action::Open);
            let open = match app.handle(Action::Edit).1 {
                Some(crate::tui::app::Effect::EditInline(open)) => open,
                other => panic!("an inline-edit effect, got {other:?}"),
            };
            app.edit_read(open, Ok("x\n".repeat(1200).into_bytes()));
            let header = frame_of(&app, w, 20).0.lines().next().unwrap().to_owned();
            assert!(
                header.contains(" · line 1/1200"),
                "{w} columns: the position survives a wide basename: {header}"
            );
            assert!(
                header.contains(".rs · line"),
                "{w} columns: the extension survives: {header}"
            );
            app.handle(Action::Editor(crate::tui::input::EditorKey::Edit(
                crate::tui::input::EditKey::Insert("x".to_owned()),
            )));
            let dirty = frame_of(&app, w, 20).0.lines().next().unwrap().to_owned();
            // `TestBackend` wraps each line in quotes.
            assert!(
                dirty
                    .trim_end_matches('"')
                    .ends_with(" · line 1/1200 · unsaved"),
                "{w} columns: `· unsaved` survives a wide basename: {dirty}"
            );
            assert!(
                dirty.starts_with(header.split(" · line").next().expect("a path")),
                "{w} columns: the path did not move:\n{header}\n{dirty}"
            );
        }
    }

    #[test]
    fn render_plural() {
        assert_eq!(plural(0, "file"), "0 files");
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(2, "root"), "2 roots");
        assert_eq!(plural(1234, "hunk"), "1,234 hunks");
        assert_eq!(count_plus(10_000, true, "file"), "10,000+ files");
        assert_eq!(count_plus(4_000, false, "file"), "4,000 files");
    }

    /// The close-out ruling: every count on screen carries thousands separators — the
    /// header, the nav branch line (with and without the row-cap `+`), the main-view
    /// header, and a row's `+a −d`.
    #[test]
    fn render_counts_at_a_thousand_carry_separators() {
        let mut app = three_roots();
        let mut pile = rows_n(1200, 0, 0);
        pile.rows[0].added = 100_000;
        pile.rows[0].deleted = 1_000;
        app.apply(pile_event("alpha", pile.clone()));
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(
            frame.contains("lastcall  3 repos · 1,203 files · "),
            "{frame}"
        );
        assert!(frame.contains("main · 1,200 files"), "{frame}");
        assert!(frame.contains("+100,000 −1,000"), "{frame}");

        pile.omitted = 50;
        app.apply(pile_event_seq("alpha", 1, pile));
        app.select(Some(Selection::Root(root("alpha"))));
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(
            frame.contains("lastcall  3 repos · 1,203+ files · "),
            "{frame}"
        );
        assert!(frame.contains("alpha  main · 1,200+ files"), "{frame}");
    }

    #[test]
    fn render_hints_and_help_follow_the_app_keymap() {
        let mut app = App::new();
        // Verifier (b) F4: an empty app has no hunk to walk to and nothing to accept, so
        // neither `n/p hunk` nor `^A accept all` is offered. `render_hints_follow_the_selection`
        // is where the full line is pinned.
        assert_eq!(
            hints(&app, 100),
            "↑↓ select  ⏎ open  t hide empty  Tab focus  r refresh  ? help  q quit"
        );
        assert_eq!(
            hints(&app, 60),
            "↑↓ select  ⏎ open  t hide empty  ? help  q quit",
            "below `NAV_MIN_COLS` the wide-frame five are gone"
        );
        for (name, specs) in &mut app.keymap {
            if name == "quit" {
                *specs = vec!["x".to_owned()];
            }
        }
        let (frame, _) = frame_of(&app, 80, 12);
        assert!(frame.contains("? help  x quit"), "{frame}");
        assert!(!frame.contains("q quit"), "{frame}");
        app.help = true;
        // 100 wide, not 80: at 80 the table has no room for a second column and the
        // overlay clips (see `render_help_uses_two_columns_only_when_one_does_not_fit`).
        // This test is about the overlay following the keymap, so it uses a frame that
        // shows every row.
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(frame.contains("x              quit"), "{frame}");
        assert!(!frame.contains("q / Ctrl-C"), "{frame}");
        assert!(
            frame.contains("A              accept the whole file or repo"),
            "{frame}"
        );
        assert!(frame.contains("y / ⏎          confirm"), "{frame}");
        assert!(frame.contains("n / Esc        cancel"), "{frame}");
        // Ruling 2: the arrows are the third spec of `open` / `back`, drawn like ↑↓.
        assert!(frame.contains("⏎ / l / →      open the diff"), "{frame}");
        assert!(frame.contains("Esc / h / ←    back"), "{frame}");
        // Ruling 3: the mouse note, until Phase 8's select-to-copy.
        assert!(frame.contains(SELECT_NOTE), "{frame}");
    }

    /// Deliverable 11 (design review F15): the four keys Phase 8 added are all on screen at
    /// 100×30, which holds only while their descriptions stay short enough for the overlay
    /// to keep two columns — `help_columns` falls back to one clipped column the moment the
    /// two widest rows plus 7 exceed the width.
    #[test]
    fn render_help_shows_the_phase8_keys_at_100x30() {
        let mut app = three_roots();
        app.help = true;
        let (frame, _) = frame_of(&app, 100, 30);
        for (key, action) in [
            ("i", "edit"),
            ("I", "edit_external"),
            ("v", "select"),
            ("y", "copy"),
        ] {
            let row = format!("{key:<15}{}", Action::describe(action));
            assert!(frame.contains(&row), "no {row:?} row:\n{frame}");
            assert!(
                Action::describe(action).width() <= 30,
                "{action} would cost the overlay its second column"
            );
        }
        assert!(frame.contains(SELECT_NOTE), "{frame}");
    }

    /// Amendment v1.11, deliverable 5. The welcome card shows itself once and then writes a
    /// marker; the only way back to it is the flag, and the one place a reader looks
    /// something up is this overlay. The row is narrower than `SELECT_NOTE`, so it costs
    /// the box no width — that is what keeps it from pushing the table into a clip.
    #[test]
    fn render_help_says_how_to_see_the_welcome_again() {
        let mut app = App::new();
        app.help = true;
        for (w, h) in [(100u16, 40u16), (100, 30), (80, 24), (60, 14)] {
            let (frame, _) = frame_of(&app, w, h);
            assert!(frame.contains(TOUR_NOTE), "{w}x{h}:\n{frame}");
        }
        assert!(
            TOUR_NOTE.width() <= SELECT_NOTE.width(),
            "the row would widen the overlay"
        );
    }

    /// Ruling P9 in the one place a reviewer looks a key up: the overlay names `⇧⏎` only
    /// on a terminal that reports the enhancement, and names the key that always works
    /// everywhere else. Which terminals report it is `docs/dev/tui.md`'s answer, not a row.
    #[test]
    fn render_help_promises_shift_enter_only_with_enhancement() {
        let mut app = App::new();
        app.help = true;
        let (plain, _) = frame_of(&app, 100, 30);
        assert!(plain.contains(NEWLINE_NOTE), "{plain}");
        assert!(
            !plain.contains("⇧⏎ or ^J"),
            "no ⇧⏎ promise without the protocol:\n{plain}"
        );
        app.enhanced = true;
        let (enhanced, _) = frame_of(&app, 100, 30);
        assert!(enhanced.contains(NEWLINE_NOTE_ENHANCED), "{enhanced}");
        // Both forms name `^J`: it is the newline that needs no terminal at all.
        assert!(NEWLINE_NOTE.contains("^J") && NEWLINE_NOTE_ENHANCED.contains("^J"));
    }

    /// Deliverable 8: the overlay goes to two columns rather than losing rows off the
    /// bottom.
    ///
    /// `take(inner.height)` truncates in silence, so a 30-line terminal showed a "keys"
    /// panel that did not list the keys. The trigger is the overflow, not the row count, so
    /// the same keymap in a taller terminal keeps the one-column form.
    #[test]
    fn render_help_uses_two_columns_only_when_one_does_not_fit() {
        let mut app = App::new();
        // 31 rows total: the keymap's own, plus filler, plus the two modal rows. Named
        // explicitly so the layout under test does not drift with the keymap's length.
        let modal = MODAL_KEYS.len();
        while app.keymap.len() + modal < 31 {
            let n = app.keymap.len();
            app.keymap
                .push((format!("filler_{n}"), vec![format!("f{n}")]));
        }
        app.keymap.truncate(31 - modal);
        assert_eq!(app.keymap.len() + modal, 31);
        app.help = true;

        // Short: two columns, and every row is on screen.
        let (frame, _) = frame_of(&app, 100, 30);
        let first = &app.keymap[0].0;
        let last_left = &app.keymap[31usize.div_ceil(2) - 1].0;
        let first_right = &app.keymap[31usize.div_ceil(2)].0;
        let row_of = |name: &str| -> String {
            let d = Action::describe(name);
            frame
                .lines()
                .find(|l| l.contains(d) && !d.is_empty())
                .unwrap_or_else(|| panic!("no row for {name} ({d:?}) in\n{frame}"))
                .to_owned()
        };
        // The first row of each column shares a line: order runs down, then across.
        let top = row_of(first);
        assert!(
            top.contains(Action::describe(first_right)),
            "column one's first row and column two's first row share a line:\n{top}"
        );
        assert!(
            !row_of(last_left).contains(Action::describe(first)),
            "and the columns are not one long row"
        );
        for (name, _) in &app.keymap {
            let d = Action::describe(name);
            if !d.is_empty() {
                assert!(
                    frame.contains(d),
                    "row {name:?} fell off the overlay:\n{frame}"
                );
            }
        }
        assert!(frame.contains(SELECT_NOTE), "{frame}");
        assert!(frame.contains("any key closes"), "{frame}");

        // Tall: the same rows fit in one column, so nothing is doubled up.
        let (tall, _) = frame_of(&app, 100, 45);
        let top = tall
            .lines()
            .find(|l| l.contains(Action::describe(first)))
            .unwrap();
        assert!(
            !top.contains(Action::describe(first_right)),
            "one column at 45 lines:\n{top}"
        );
        for (name, _) in &app.keymap {
            let d = Action::describe(name);
            if !d.is_empty() {
                assert!(tall.contains(d), "row {name:?} missing:\n{tall}");
            }
        }

        // Narrow and short: one column is all there is room for, truncation and all.
        let keys: Vec<String> = (0..31).map(|i| format!("k{i:<12} does a thing")).collect();
        assert_eq!(
            help_columns(&keys, Rect::new(0, 0, 40, 30)).len(),
            31,
            "40 columns cannot hold two"
        );
        assert_eq!(help_columns(&keys, Rect::new(0, 0, 100, 30)).len(), 16);

        // Verifier (b) F4. 80 columns is the standard width and has no room for a second
        // column (the widest left row plus the widest right row plus the gutter and the
        // border come to 100), so the overlay clips there. What it clips is **key rows**:
        // the mouse note and `any key closes` are reserved out of the truncation, because a
        // reader who cannot see every key still has to be able to leave.
        let mut narrow = App::new();
        narrow.help = true;
        for (w, h) in [(80u16, 30u16), (80, 24)] {
            let (frame, _) = frame_of(&narrow, w, h);
            assert!(frame.contains("any key closes"), "{w}x{h}:\n{frame}");
            assert!(frame.contains(SELECT_NOTE), "{w}x{h}:\n{frame}");
            // Amendment v1.11: the welcome runs once, so the row that says how to get it
            // back is reserved out of the clip beside the other two.
            assert!(frame.contains(TOUR_NOTE), "{w}x{h}:\n{frame}");
        }
        // At 30 lines the clip stops after the quit row — the one a reader who opened the
        // overlay by accident needs most.
        let (frame, _) = frame_of(&narrow, 80, 30);
        assert!(frame.contains("q / Ctrl-C     quit"), "{frame}");

        // Design pass D12 / ruling R12: the clip says so. The last body row above the
        // pinned `quit` is a dim `… N more keys (100 columns shows all)`, where N is the
        // rows that are not on the frame and the column figure is computed from the width
        // the two-column form needs — not a literal, so it follows the keymap.
        let all: Vec<String> = narrow
            .keymap
            .iter()
            .map(|(name, specs)| (name.as_str(), keys_label(specs)))
            .chain(
                MODAL_KEYS
                    .iter()
                    .map(|(name, specs)| (*name, keys_label(specs))),
            )
            .map(|(name, keys)| format!("{keys:<14} {}", Action::describe(name)))
            .collect();
        for (w, h) in [(80u16, 30u16), (80, 24)] {
            let (frame, styles) = frame_of(&narrow, w, h);
            let lines: Vec<&str> = frame.lines().collect();
            let at = lines
                .iter()
                .position(|l| l.contains("more key"))
                .unwrap_or_else(|| panic!("{w}x{h}: a clip notice:\n{frame}"));
            let notice = lines[at];
            assert!(
                notice.contains(&format!(
                    "({} columns shows all)",
                    help_two_column_width(&all)
                )),
                "{w}x{h}: {notice}"
            );
            assert!(
                lines[at + 1].contains("q / Ctrl-C     quit"),
                "{w}x{h}: the notice sits directly above the pinned quit:\n{frame}"
            );
            let hidden: usize = notice
                .split_whitespace()
                .find_map(|w| w.parse().ok())
                .unwrap_or_else(|| panic!("{w}x{h}: a count in {notice:?}"));
            let shown = all.iter().filter(|r| frame.contains(r.as_str())).count();
            assert_eq!(
                shown + hidden,
                all.len(),
                "{w}x{h}: every key row is on the frame or counted:\n{frame}"
            );
            // Dim: it is the overlay talking about itself, not a key.
            assert!(
                styles
                    .lines()
                    .any(|l| l.starts_with(&format!("{at} ")) && l.contains("DIM")),
                "{w}x{h}: the notice on row {at} is dim:\n{styles}"
            );
        }

        // Verifier (b) F1: in two columns a body row carries two keys, and the count was
        // of rows — `2 more keys` over four hidden ones at 100×20. The identity holds in
        // both forms, and at the two-column width the remedy names the other dimension.
        for (w, h) in [(100u16, 20u16), (100, 14), (120, 12)] {
            let (frame, _) = frame_of(&narrow, w, h);
            let lines: Vec<&str> = frame.lines().collect();
            let notice = lines
                .iter()
                .find(|l| l.contains("more key"))
                .unwrap_or_else(|| panic!("{w}x{h}: a clip notice:\n{frame}"));
            assert!(
                notice.contains("(a taller window shows all)"),
                "{w}x{h}: already as wide as two columns need: {notice}"
            );
            let hidden: usize = notice
                .split_whitespace()
                .find_map(|w| w.parse().ok())
                .unwrap_or_else(|| panic!("{w}x{h}: a count in {notice:?}"));
            let shown = all.iter().filter(|r| frame.contains(r.as_str())).count();
            assert_eq!(
                shown + hidden,
                all.len(),
                "{w}x{h}: every key is on the frame or counted:\n{frame}"
            );
            assert!(frame.contains("quit"), "{w}x{h}: quit stays:\n{frame}");
        }
    }

    /// Under the modal the hint line names only the keys that work there: the modal's
    /// own (fixed) and the keymap's `quit` key — not the accepts and the help.
    #[test]
    fn render_hint_line_under_the_modal_names_only_its_keys() {
        let mut app = three_roots();
        app.apply(pile_event("alpha", rows_n(11, 0, 0)));
        app.select(Some(Selection::Root(root("alpha"))));
        assert_eq!(app.handle(Action::AcceptFile), (Changed::Yes, None));
        assert!(app.confirm.is_some());
        assert_eq!(hints(&app, 100), "y confirm  n cancel  q quit");
        assert_eq!(
            hints(&app, 40),
            "y confirm  n cancel  q quit",
            "never shrinks"
        );
        let (frame, _) = frame_of(&app, 100, 30);
        let last = frame.lines().last().unwrap();
        assert!(last.contains("y confirm  n cancel  q quit"), "{last}");
        assert!(!last.contains("accept all"), "{last}");
        // The quit key is the user's own; the modal keys are fixed.
        for (name, specs) in &mut app.keymap {
            if name == "quit" {
                *specs = vec!["ctrl-x".to_owned()];
            }
        }
        assert_eq!(hints(&app, 100), "y confirm  n cancel  ^X quit");
        app.handle(Action::Cancel);
        assert!(
            hints(&app, 100).contains("A accept all in alpha  ^A accept all"),
            "{}",
            hints(&app, 100)
        );
    }

    #[test]
    fn render_hints_follow_the_selection() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        // The ruling: `a accept hunk` on a file row in BOTH panes — this one is the nav.
        assert_eq!(app.effective_focus(), super::super::app::Focus::Nav);
        let nav_line = hints(&app, 124);
        assert_eq!(
            nav_line,
            "↑↓ select  ⏎ open  n/p hunk  a accept hunk  A accept file  ^A accept all  t hide empty  Tab focus  r refresh  ? help  q quit"
        );
        // Phase 13: with eight more columns the line also says what `c` will do, and the
        // verb follows the state. `wrap` is **first** in `HINT_DROP_ORDER`, so it is the
        // first hint off the line and every width below this one reads exactly as it did
        // before the phase (design review F25) — which is what the rest of this test,
        // unchanged from Phase 12, pins.
        assert_eq!(
            hints(&app, 132),
            "↑↓ select  ⏎ open  n/p hunk  a accept hunk  A accept file  ^A accept all  t hide empty  Tab focus  r refresh  c clip  ? help  q quit"
        );
        assert_eq!(
            hints(&app, 200),
            hints(&app, 132),
            "132 is the whole nav line"
        );
        assert_eq!(hints(&app, 131), nav_line, "the wrap hint goes first");
        app.handle(Action::ToggleWrap);
        assert!(
            hints(&app, 132).contains("c wrap"),
            "the verb follows the state: {}",
            hints(&app, 132)
        );
        app.handle(Action::ToggleWrap);
        // Ruling R4: one hint at a time, from the right end of `HINT_DROP_ORDER` — so a
        // column short of the whole line the nav keeps everything but `r refresh`.
        assert_eq!(
            hints(&app, 123),
            "↑↓ select  ⏎ open  n/p hunk  a accept hunk  A accept file  ^A accept all  t hide empty  Tab focus  ? help  q quit"
        );
        assert_eq!(
            hints(&app, 100),
            "↑↓ select  ⏎ open  n/p hunk  a accept hunk  A accept file  t hide empty  ? help  q quit",
            "`Tab focus` then `^A accept all`; the toggle outlives both"
        );
        assert_eq!(
            hints(&app, 80),
            "↑↓ select  ⏎ open  n/p hunk  a accept hunk  A accept file  ? help  q quit",
            "then the toggle"
        );
        assert_eq!(
            hints(&app, 60),
            "↑↓ select  ⏎ open  n/p hunk  a accept hunk  ? help  q quit",
            "then the file accept — and below 70 the wide-frame five were never offered"
        );
        app.handle(Action::Open);
        // Phase 8 deliverable 9: `v select  y copy` are the diff pane's own keys and the
        // first two off `HINT_DROP_ORDER`, so a frame that loses them keeps `r refresh`
        // and everything under it — no narrower frame loses a hint it used to have.
        assert_eq!(
            hints(&app, 142),
            "↑↓ scroll  ← back  n/p hunk  a accept hunk  A accept file  ^A accept all  t hide empty  Tab focus  r refresh  v select  y copy  ? help  q quit"
        );
        assert_eq!(
            hints(&app, 150),
            hints(&app, 200),
            "150 is the whole line once the wrap hint is on it"
        );
        assert_eq!(
            hints(&app, 142),
            hints(&app, 149),
            "and it is the first off it"
        );
        assert_eq!(
            hints(&app, 141),
            "↑↓ scroll  ← back  n/p hunk  a accept hunk  A accept file  ^A accept all  t hide empty  Tab focus  r refresh  v select  ? help  q quit",
            "`y copy` is the first hint off the line"
        );
        assert_eq!(
            hints(&app, 133),
            nav_line.replace("↑↓ select  ⏎ open", "↑↓ scroll  ← back"),
            "then `v select`, and the nav's own line with the diff's opening is what is left"
        );
        assert_eq!(
            hints(&app, 133).width(),
            nav_line.width(),
            "`↑↓ scroll` and `← back` are exactly as wide as `↑↓ select` and `⏎ open`, so no step moved"
        );
        // Deliverable 3 (D2 / ruling R3): `←` is `back`'s own arrow spelling, chosen over
        // its first spec (`Esc`) so tier 0 reads as the pair of arrows it is; a keymap that
        // binds no arrow to `back` falls back to whatever its first key is.
        let mut rebound = app.clone();
        for (name, specs) in &mut rebound.keymap {
            if name == "back" {
                *specs = vec!["esc".to_owned()];
            }
        }
        assert!(
            hints(&rebound, 200).starts_with("↑↓ scroll  Esc back"),
            "{}",
            hints(&rebound, 200)
        );
        // What the line says depends on the selection: a root row trades
        // `a accept hunk  A accept file` for `A accept all in <root>`. Verifier (b) F4: it
        // also carries no hunks, so with the diff focused it is offered neither `n/p hunk`
        // nor the diff pane's `v select  y copy` — at any width, since a hint that answers
        // nothing is not a hint the line is short of.
        let mut at_root = app.clone();
        at_root.select(Some(Selection::Root(root("alpha"))));
        for hint in ["n/p hunk", "v select", "y copy"] {
            assert!(
                !hints(&at_root, 200).contains(hint),
                "`{hint}` on a root row: {}",
                hints(&at_root, 200)
            );
        }
        assert!(
            hints(&at_root, 200).contains("A accept all in alpha"),
            "{}",
            hints(&at_root, 200)
        );
        assert!(!nav_line.contains("y copy"), "the nav has no copy key");
        app.handle(Action::Back);
        assert!(
            !hints(&app, 200).contains("y copy"),
            "and the nav still has none at any width"
        );
        app.handle(Action::Open);
        app.select(Some(Selection::Root(root("alpha"))));
        assert!(
            hints(&app, 100).contains("A accept all in alpha  ^A accept all"),
            "{}",
            hints(&app, 100)
        );
        app.select(Some(Selection::Group(
            root("beta"),
            lastcall_engine::scan::Annotation::Upstream,
        )));
        assert!(
            hints(&app, 100).contains("A accept group  ^A accept all"),
            "{}",
            hints(&app, 100)
        );
    }

    /// Verifier (b) F4: the line never promises a key that would do nothing.
    ///
    /// The three cases the review found, each checked against the key itself rather than
    /// against a remembered string: press it and nothing changes, so the hint is gone.
    #[test]
    fn render_hints_never_promise_a_key_that_answers_nothing() {
        // `n`/`p` on a repo row: no hunks to walk.
        let mut at_root = three_roots();
        at_root.select(Some(Selection::Root(root("alpha"))));
        assert!(at_root.view_hunks().is_empty());
        assert_eq!(at_root.clone().handle(Action::HunkNext).0, Changed::No);
        assert!(
            !hints(&at_root, 200).contains("hunk"),
            "{}",
            hints(&at_root, 200)
        );
        // …and a file row with hunks still has it, so the gate is the hunks and not the
        // pane: every hint's converse holds.
        let mut at_file = three_roots();
        at_file.apply(pile_event_seq("alpha", 1, alpha_two_hunks()));
        at_file.select(Some(row("alpha", "f1")));
        assert_eq!(at_file.clone().handle(Action::HunkNext).0, Changed::Yes);
        assert!(
            hints(&at_file, 200).contains("n/p hunk"),
            "{}",
            hints(&at_file, 200)
        );

        // `^A` with nothing pending anywhere: the key answers `nothing to accept`.
        let mut clean = three_roots();
        for (name, paths) in [
            ("alpha", &["f1", "f2", "src/parse.rs"][..]),
            ("beta", &["u1", "u2"][..]),
            ("notes", &["n2.md"][..]),
        ] {
            clean.apply(pile_event_seq(name, 1, without(pile(name), paths)));
        }
        assert_eq!(clean.counts_of(&AcceptScope::All).files, 0);
        assert!(
            !hints(&clean, 200).contains("accept all"),
            "{}",
            hints(&clean, 200)
        );
        assert!(
            hints(&at_file, 200).contains("^A accept all"),
            "{}",
            hints(&at_file, 200)
        );

        // `v`/`y` on a hunkless file with the diff focused: there is no line to select.
        let mut collapsed = three_roots();
        collapsed.handle(Action::Resize(100, 30));
        collapsed.apply(pile_event_seq("alpha", 1, alpha_collapsed(Collapsed::Glob)));
        collapsed.select(Some(row("alpha", "f1")));
        collapsed.handle(Action::Open);
        assert_eq!(collapsed.effective_focus(), Focus::Diff);
        assert!(collapsed.view_hunks().is_empty());
        assert_eq!(collapsed.clone().handle(Action::Select).0, Changed::No);
        for hint in ["v select", "y copy"] {
            assert!(
                !hints(&collapsed, 200).contains(hint),
                "`{hint}` on a hunkless row: {}",
                hints(&collapsed, 200)
            );
        }
        let mut with_hunks = three_roots();
        with_hunks.handle(Action::Resize(100, 30));
        with_hunks.select(Some(row("alpha", "f1")));
        with_hunks.handle(Action::Open);
        assert!(
            hints(&with_hunks, 200).contains("v select  y copy"),
            "{}",
            hints(&with_hunks, 200)
        );
    }

    /// Verifier (b) F5: `any key closes` is on the overlay at **every** height it draws at,
    /// including the one where the table fits exactly.
    ///
    /// The old draw wrote the line only where the body fell short of the box, so at the
    /// single height where `rows.len()` equalled the inner height the way out went unsaid —
    /// one row shorter it came back, together with the clip notice. A sweep rather than the
    /// two sizes the review named: the exact-fit height moves with the keymap.
    #[test]
    fn render_help_says_any_key_closes_at_every_height() {
        let mut app = three_roots();
        app.help = true;
        for width in [60u16, 80, 100, 140] {
            for height in MIN_SIZE.1..=48 {
                let (frame, _) = frame_of(&app, width, height);
                assert!(
                    frame.contains("any key closes"),
                    "{width}x{height}:\n{frame}"
                );
            }
        }
    }

    /// Ruling R4 (the sponsor: "trying our best to fit everything but once it gets beyond
    /// N columns, cut it back and add `? help`"): the line loses **one** hint per column
    /// step, in `HINT_DROP_ORDER`, and never a fixed tier at once. Walking one column at a
    /// time from the whole line to the floor, every step either changes nothing or removes
    /// exactly one hint, and the hint it removes is the next one still on the line.
    #[test]
    fn render_hints_drop_one_at_a_time_from_the_right() {
        use crate::tui::herdr::{HerdrUpdate, Scope};
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);
        app.herdr.scoped = true;
        app.handle(Action::Herdr(HerdrUpdate::Scope(Some(Scope {
            label: "w1".to_owned(),
            roots: [root("alpha"), root("beta"), root("notes")]
                .into_iter()
                .collect(),
        }))));
        let whole = hints(&app, 400);
        assert!(
            whole.contains("w scope") && whole.contains("y copy"),
            "{whole}"
        );
        let parts = |line: &str| line.split("  ").map(|s| s.to_owned()).collect::<Vec<_>>();
        let mut previous = parts(&whole);
        // From the whole line down to `MIN_SIZE`'s floor. `NAV_MIN_COLS` is the one step
        // that may take several at once (the five hints a nav-less frame cannot promise).
        for width in (40..=whole.width() as u16).rev() {
            let now = parts(&hints(&app, width));
            assert!(
                now.iter().all(|h| previous.contains(h)),
                "no hint returns as the line narrows: {now:?} after {previous:?}"
            );
            if width + 1 != NAV_MIN_COLS {
                assert!(
                    previous.len() - now.len() <= 1,
                    "one at a time at {width}: {now:?} after {previous:?}"
                );
            }
            if previous.len() != now.len() {
                let gone: Vec<&String> = previous.iter().filter(|h| !now.contains(h)).collect();
                let expected = HINT_DROP_ORDER
                    .iter()
                    .find(|name| previous.iter().any(|h| hint_is(h, name, &app)));
                assert!(
                    expected.is_some_and(|name| gone.iter().any(|h| hint_is(h, name, &app))),
                    "the next one in the drop order goes at {width}: {gone:?}"
                );
            }
            previous = now;
        }
        assert_eq!(previous, parts("↑↓ scroll  ← back  ? help  q quit"));
    }

    /// Which hint a rendered fragment is, by the key its action is bound to — the drop
    /// order is action names and the line is labels.
    fn hint_is(rendered: &str, action: &str, app: &App) -> bool {
        let key = match action {
            "hunk_next" => "n/p".to_owned(),
            other => app
                .keys_for(other)
                .first()
                .map(|s| hint_label(s))
                .unwrap_or_default(),
        };
        !key.is_empty() && rendered.starts_with(&format!("{key} "))
    }

    /// Ruling R4's own sentence: `? help  q quit` are always the last two hints on the
    /// line, so a cut line still says where the rest of the keys are. Every width from
    /// `MIN_SIZE`'s 40 to 70 — the widths D1's constants would have left with a line that
    /// simply did not fit.
    #[test]
    fn render_hints_keep_help_and_quit_at_every_width() {
        use crate::tui::herdr::{HerdrUpdate, Scope};
        let mut app = three_roots();
        app.herdr.scoped = true;
        app.handle(Action::Herdr(HerdrUpdate::Scope(Some(Scope {
            label: "w1".to_owned(),
            roots: [root("alpha")].into_iter().collect(),
        }))));
        for selection in [
            None,
            Some(row("alpha", "f1")),
            Some(Selection::Root(root("alpha"))),
        ] {
            app.select(selection.clone());
            for width in 40..=70u16 {
                let line = hints(&app, width);
                assert!(
                    line.ends_with("? help  q quit"),
                    "{width} ({selection:?}): {line}"
                );
                assert!(
                    line.width() <= width as usize,
                    "{width} ({selection:?}) is {} wide: {line}",
                    line.width()
                );
            }
        }
    }

    /// The 80-column frame is the one the release's readers are likeliest to have, and
    /// `A accept file` is the hint that tells a file row from a hunk. The drop order is
    /// built so it survives there: `y`, `v`, `r`, `Tab`, `w`, `^A` and `t` all go first.
    #[test]
    fn render_hints_at_80_keep_accept_file() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        let line = hints(&app, 80);
        assert!(line.contains("a accept hunk  A accept file"), "{line}");
        assert!(line.ends_with("? help  q quit"), "{line}");
        assert!(line.width() <= 80, "{line}");
        app.handle(Action::Open);
        assert_eq!(
            hints(&app, 80),
            line.replace("↑↓ select  ⏎ open", "↑↓ scroll  ← back"),
            "the diff pane says the same at 80 but for its own tier 0 (deliverable 3)"
        );
    }

    #[test]
    fn render_accept_controls_are_targets() {
        let mut app = three_roots();
        app.apply(pile_event("alpha", alpha_two_hunks()));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut hits = HitMap::default();
        terminal
            .draw(|f| {
                hits = render(&app, f);
            })
            .unwrap();
        let frame = terminal.backend().to_string();
        assert!(frame.contains("  [Accept All]"), "{frame}");
        assert!(frame.contains("[A accept file]"), "{frame}");
        assert_eq!(frame.matches("[a accept]").count(), 2, "{frame}");
        let rect_of = |t: &Target| {
            hits.targets
                .iter()
                .find(|(_, x)| x == t)
                .map(|(r, _)| *r)
                .unwrap_or_else(|| panic!("{t:?} on screen"))
        };
        let all = rect_of(&Target::HeaderAcceptAll);
        assert_eq!((all.y, all.width), (0, 12));
        assert_eq!(hits.at(all.x, 0), Some(&Target::HeaderAcceptAll));
        let file = rect_of(&Target::FileAccept);
        assert_eq!(hits.at(file.right() - 1, file.y), Some(&Target::FileAccept));
        let h1 = rect_of(&Target::HunkAccept(1));
        assert_eq!(hits.at(h1.x, h1.y), Some(&Target::HunkAccept(1)));
        assert_eq!(hits.at(h1.x - 3, h1.y), Some(&Target::DiffHunk(1)));
        assert_eq!(hits.at(file.x - 3, file.y), Some(&Target::DiffBody));
        // The control label follows the keymap.
        for (name, specs) in &mut app.keymap {
            if name == "accept" {
                *specs = vec!["z".to_owned()];
            }
        }
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(frame.contains("[z accept]"), "{frame}");
    }

    /// Verifier (b) F5: an expansion hunk header carries `[m flag]` and nothing else.
    ///
    /// `m` there quotes the hunk under the cursor, so the control that says so is drawn and
    /// registered. Accept stays off (a collapsed row is a single accept, §6.3) and restore
    /// with it — the row carries no hunks, so `[u restore]` would open `Restore f1 · 0
    /// hunks?`. Whole-file restore is still `U`.
    #[test]
    fn render_expansion_hunks_offer_flag_but_no_accept_or_restore() {
        let mut app = three_roots();
        app.apply(pile_event_seq("alpha", 1, alpha_collapsed(Collapsed::Glob)));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);
        let asked = app.selected_row().expect("f1").clone();
        app.set_expanded(root("alpha"), &asked, expansion_of(3, 0));

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut hits = HitMap::default();
        terminal
            .draw(|f| {
                hits = render(&app, f);
            })
            .unwrap();
        let frame = terminal.backend().to_string();
        assert!(frame.contains("collapsed (glob)"), "{frame}");
        assert_eq!(
            frame.matches("[m flag]").count(),
            3,
            "one per hunk:\n{frame}"
        );
        assert!(!frame.contains("[a accept]"), "{frame}");
        assert!(!frame.contains("[u restore]"), "{frame}");

        let has = |t: Target| hits.targets.iter().any(|(_, x)| *x == t);
        for h in 0..3 {
            assert!(has(Target::HunkFlag(h)), "hunk {h} has a flag target");
            assert!(
                !has(Target::HunkRestore(h)),
                "hunk {h} has no restore target"
            );
            assert!(!has(Target::HunkAccept(h)), "hunk {h} has no accept target");
        }
        // A click on the control flags that hunk, not the file.
        let (rect, _) = hits
            .targets
            .iter()
            .find(|(_, t)| *t == Target::HunkFlag(1))
            .expect("hunk 2's control");
        assert_eq!(hits.at(rect.x, rect.y), Some(&Target::HunkFlag(1)));
    }

    /// The sponsor's ruling: exactly one blank line between consecutive hunks (none before
    /// the first, none after the last), and the selected hunk's `@@ … @@` header is a
    /// full-width band across the diff pane rather than two islands of inverse.
    #[test]
    fn render_hunks_are_separated_and_the_current_header_is_a_band() {
        let mut app = three_roots();
        app.apply(pile_event("alpha", alpha_two_hunks()));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut hits = HitMap::default();
        terminal
            .draw(|f| {
                hits = render(&app, f);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let main = hits.main.expect("the main pane");
        let text = |y: u16| -> String {
            (main.x..main.right())
                .map(|x| buf[(x, y)].symbol())
                .collect()
        };
        let headers: Vec<u16> = (main.y..main.bottom())
            .filter(|y| text(*y).contains("@@ -"))
            .collect();
        assert_eq!(headers.len(), 2, "{}", terminal.backend());
        let (first, second) = (headers[0], headers[1]);
        assert!(
            text(second - 1).trim().is_empty(),
            "a blank line before the second hunk: {:?}",
            text(second - 1)
        );
        assert!(
            !text(second - 2).trim().is_empty(),
            "exactly one blank line, not two: {:?}",
            text(second - 2)
        );
        // None after the last hunk: two hunks cost their own lines plus one separator.
        let selected = app.selected_row().expect("f1");
        let own: usize = selected
            .hunks
            .iter()
            .map(super::super::app::hunk_height)
            .sum();
        assert_eq!(diff_len(selected), own + 1);
        // The current hunk's header is one style run across the whole pane; the other
        // header is not inverted at all.
        assert!(
            styles(&buf).contains(&format!(
                "{first} {}..{} Reset Reset REVERSED\n",
                main.x,
                main.right()
            )),
            "{}",
            styles(&buf)
        );
        assert!(
            !buf[(main.x, second)].modifier.contains(Modifier::REVERSED),
            "only the selected header is a band"
        );
    }

    /// Deliverable 9: the selected lines are one reverse-video run across the pane, the
    /// hunk lines' rectangle is reported for the mouse, and the cue sits over the diff.
    #[test]
    fn render_selection_is_reverse_video_and_the_cue_sits_over_the_diff() {
        let mut app = three_roots();
        app.apply(pile_event("alpha", alpha_two_hunks()));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);
        app.handle(Action::Resize(100, 30));
        // Lines 1..=3 of the diff: `-a1`, `+A1`, ` a2` — not the header, so the run cannot
        // be mistaken for the selected-hunk band.
        app.handle(Action::NavDown);
        app.handle(Action::Select);
        app.handle(Action::NavDown);
        app.handle(Action::NavDown);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut hits = None;
        terminal.draw(|f| hits = Some(render(&app, f))).unwrap();
        let hits = hits.unwrap();
        let buf = terminal.backend().buffer().clone();
        let body = hits.diff_body.expect("the hunk lines' rectangle");
        let main = hits.main.expect("the diff pane");
        assert!(main.union(body) == main, "the body is inside the pane");
        // The first `j` scrolled (no selection yet), the two after it only moved the
        // selection's far end — so line 1 is the top row and the selection is the three
        // rows from there.
        assert_eq!(app.diff.scroll, 1);
        assert_eq!(app.sel.map(|s| s.range()), Some((1, 3)));
        // Every cell of every selected row, edge to edge — the underlying `+`/`-` colours
        // stay, so the run is one *modifier* across the pane rather than one style span.
        let styled = styles(&buf);
        for y in body.y..body.y + 3 {
            for x in body.x..body.right() {
                assert!(
                    buf[(x, y)].modifier.contains(Modifier::REVERSED),
                    "({x}, {y}) is not selected:\n{styled}"
                );
            }
        }
        assert!(
            !buf[(body.x, body.y + 3)]
                .modifier
                .contains(Modifier::REVERSED),
            "and the line after it is not selected"
        );

        // The cue is centred over the diff pane and says so.
        app.handle(Action::Copy);
        let (frame, _) = frame_of(&app, 100, 30);
        let cue = frame
            .lines()
            .find(|l| l.contains(super::super::app::COPIED))
            .expect(&frame);
        let at = cue.find(super::super::app::COPIED).unwrap() as u16;
        assert!(
            at > main.x && at < main.right(),
            "the cue is over the diff pane, not the nav: {cue:?}"
        );
    }

    #[test]
    fn render_confirm_modal_shows_live_counts() {
        let mut app = three_roots();
        app.apply(pile_event("alpha", rows_n(11, 2, 1)));
        app.select(Some(Selection::Root(root("alpha"))));
        assert_eq!(app.handle(Action::AcceptFile), (Changed::Yes, None));
        let (frame, styles) = frame_of(&app, 100, 30);
        assert!(frame.contains("Accept all 11 files in alpha?"), "{frame}");
        assert!(
            frame.contains("2 grouped upstream · 1 collapsed"),
            "{frame}"
        );
        assert!(frame.contains("y / ⏎ confirm    n / Esc cancel"), "{frame}");
        assert!(frame.contains(" accept "), "{frame}");
        assert!(styles.contains("BOLD"), "{styles}");
        // A pile applied underneath changes the number shown.
        app.apply(pile_event_seq("alpha", 1, rows_n(12, 0, 0)));
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(frame.contains("Accept all 12 files in alpha?"), "{frame}");
        assert!(!frame.contains("grouped upstream"), "{frame}");
        // Global scope names the repos.
        app.handle(Action::Cancel);
        assert_eq!(app.handle(Action::AcceptAll), (Changed::Yes, None));
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(
            frame.contains("Accept all 15 files across 3 repos?"),
            "{frame}"
        );
    }

    #[test]
    fn render_truncated_counts_carry_a_plus() {
        let mut app = three_roots();
        let mut pile = pile("alpha");
        pile.omitted = 7;
        pile.notices
            .push("3 files shown · 7 more changed paths not scanned".to_owned());
        app.apply(pile_event("alpha", pile));
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(frame.contains("lastcall  3 repos · 6+ files"), "{frame}");
        assert!(frame.contains("main · 3+ files"), "{frame}");
        assert!(
            frame.contains("main · 2 files"),
            "beta stays plain: {frame}"
        );
        app.select(Some(Selection::Root(root("alpha"))));
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(frame.contains("alpha  main · 3+ files"), "{frame}");
        assert!(frame.contains("7 more changed paths"), "{frame}");
    }

    #[test]
    fn render_in_progress_tag_decorates_a_listed_root() {
        let mut app = three_roots();
        let mut alpha = meta("alpha");
        alpha.in_progress = Some(lastcall_engine::headstate::InProgress::Merge);
        app.sync_roots(vec![alpha, meta("beta"), meta("notes")]);
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(frame.contains("│alpha  [merge in progress]"), "{frame}");
        assert!(frame.contains("│  M f1  +1 −1"), "{frame}");
    }

    #[test]
    fn render_many_notices_do_not_overflow_a_small_pane() {
        let mut app = three_roots();
        app.roots.get_mut(&root("alpha")).unwrap().pile.notices =
            (0..12).map(|i| format!("notice {i}")).collect();
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);
        for (w, h) in [(40, 10), (70, 10), (40, 12)] {
            let (frame, _) = frame_of(&app, w, h);
            assert!(frame.contains("notice 0"), "{frame}");
        }
        app.select(Some(Selection::Root(root("alpha"))));
        frame_of(&app, 40, 10);
    }

    #[test]
    fn render_empty_app_shows_empty_state_and_hints() {
        let app = App::new();
        let (frame, styles) = frame_of(&app, 100, 12);
        assert!(frame.contains("nothing pending across 0 repos"), "{frame}");
        assert!(
            frame.contains("lastcall  0 repos · 0 files · 0 hunks"),
            "{frame}"
        );
        assert!(frame.contains("watching nothing"), "{frame}");
        assert!(frame.contains("↑↓ select"), "{frame}");
        assert!(styles.contains("0 0..37 Reset Reset BOLD"), "{styles}");
        // counts, then the dim `standalone` badge, then the control.
        assert!(
            styles.contains("0 39..49 Reset Reset DIM"),
            "the herdr badge is dim while standalone: {styles}"
        );
        assert!(
            styles.contains("0 51..63 Reset Reset DIM"),
            "Accept All is dim with nothing listed: {styles}"
        );
    }

    /// The header's four segments do not fit at 80 columns, and the ladder is
    /// counts → notice → badge → control: `^A` and the hint line duplicate the control,
    /// nothing else says what is watched or whether herdr is answering. Design pass D6 adds
    /// one step in front: the update notice goes **before** the control does, so a reader
    /// who never asked about releases loses nothing that was there yesterday.
    #[test]
    fn render_header_drops_the_accept_control_before_the_herdr_badge() {
        let mut app = App::new();
        app.update_available("0.1.1".to_owned());
        let (wide, _) = frame_of(&app, 100, 12);
        assert!(wide.contains("↑ 0.1.1"), "{wide}");
        assert!(wide.contains("[Accept All]"), "{wide}");
        let (frame, _) = frame_of(&app, 80, 12);
        assert!(
            !frame.contains("↑ 0.1.1"),
            "the update notice is the first thing dropped: {frame}"
        );
        assert!(frame.contains("standalone"), "{frame}");
        assert!(frame.contains("watching nothing"), "{frame}");
        assert!(!frame.contains("[Accept All]"), "{frame}");
        // Nothing under the width of the counts alone survives but the counts.
        let (narrow, _) = frame_of(&app, 40, 12);
        assert!(narrow.contains("lastcall  0 repos"), "{narrow}");
        // With no update to show, every width draws exactly what it drew before.
        let plain = App::new();
        for width in [40, 80, 100, 120] {
            assert_eq!(
                frame_of(&plain, width, 12).0,
                frame_of(&App::new(), width, 12).0
            );
        }
    }

    /// Design pass D6: the notice is seven dim columns after `[Accept All]`, it is a hit
    /// target, and the click puts the sentence on the status line — the badge's
    /// click-to-read pattern, so nothing is said automatically.
    #[test]
    fn render_update_notice_is_seven_columns_after_the_control_and_a_target() {
        let mut app = App::new();
        assert!(App::new().update.is_none(), "off until the check answers");
        app.update_available("0.1.1".to_owned());
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let mut hits = HitMap::default();
        terminal
            .draw(|f| {
                hits = render(&app, f);
            })
            .unwrap();
        let frame = terminal.backend().to_string();
        assert!(frame.contains("[Accept All]  ↑ 0.1.1"), "{frame}");
        let rect = hits
            .targets
            .iter()
            .find(|(_, t)| *t == Target::HeaderUpdate)
            .map(|(r, _)| *r)
            .expect("the notice is a target");
        assert_eq!(rect.width, 7, "`↑ 0.1.1` is seven columns");
        assert_eq!(rect.y, 0);
        assert_eq!(hits.at(rect.x, 0), Some(&Target::HeaderUpdate));
        // Nothing on the status line until the reader asks for it.
        assert!(app.status.is_none(), "{:?}", app.status);
        let mut clicked = app.clone();
        clicked.hit(Target::HeaderUpdate);
        assert_eq!(
            clicked.status.as_ref().map(|s| s.text.as_str()),
            Some("lastcall 0.1.1 available — run: lastcall update")
        );
        // The same answer twice is not a repaint.
        let mut again = app.clone();
        assert_eq!(again.update_available("0.1.1".to_owned()), Changed::No);
        assert_eq!(again.update_available("0.2.0".to_owned()), Changed::Yes);
    }

    /// The loading pane (Gate 8 sponsor run ruling): a static line in the first second, no
    /// digits; from one second the counter line and a ✓ in the leading column of each
    /// reported root (Design pass D4, ruling R6). Design pass D3
    /// (ruling R5): **repos** is the noun, the counter line does not repeat it, and the
    /// header says `3 repos · checking status…` rather than three counts it does not know.
    #[test]
    fn render_loading_pane_counts_only_after_one_second() {
        let mut app = App::new();
        app.sync_roots(vec![meta("alpha"), meta("beta"), meta("notes")]);
        app.start_loading();
        app.apply(lastcall_engine::watcher::EngineEvent::Scanned {
            root: root("alpha"),
            rows: 1200,
        });
        let (frame, _) = frame_of(&app, 100, 12);
        assert!(
            frame.contains("discovered 3 repos, checking status…"),
            "{frame}"
        );
        assert!(
            !frame.contains("checked"),
            "no digits in the first second: {frame}"
        );
        assert!(!frame.contains('✓'), "{frame}");
        assert!(!frame.contains("nothing pending"), "{frame}");
        assert!(
            frame.contains("lastcall  3 repos · checking status…"),
            "D3: the header says the count it knows and no counts it does not: {frame}"
        );
        assert!(
            !frame.contains("0 files · 0 hunks"),
            "and not the counts it does not: {frame}"
        );

        app.handle(Action::Tick);
        let (frame, _) = frame_of(&app, 100, 12);
        assert!(
            frame.contains("1 of 3 checked · 1,200 files pending so far · 1s"),
            "{frame}"
        );
        // Design pass D4 (ruling R6): the tick is a leading column in the row's own
        // indent, so the ticks line up whatever the branch labels are and the root still
        // being scanned is the gap in the column.
        assert!(frame.contains("✓ alpha  main"), "{frame}");
        assert!(frame.contains("  beta  main"), "{frame}");
        assert!(!frame.contains("main  ✓"), "not a suffix: {frame}");

        // The last report ends the hold: the ordinary listing.
        app.apply(lastcall_engine::watcher::EngineEvent::Scanned {
            root: root("beta"),
            rows: 0,
        });
        app.apply(pile_event("notes", pile("notes")));
        let (frame, _) = frame_of(&app, 100, 12);
        assert!(!frame.contains("checking status"), "{frame}");
        assert!(frame.contains(NO_SELECTION), "{frame}");
    }

    /// Design pass D5 (ruling R7): the scope wait used to replace the hold with a second
    /// waiting screen — the root list and its ticks vanished, the header dropped back to
    /// `0 repos · 0 files · 0 hunks`, and a moment later the listing landed. It is now the
    /// hold's own frame continuing: below `COUNTER_AFTER` nothing changes at all, and from
    /// one second the counter carries the holding clause where `so far · Ss` was, with
    /// every root ticked because every root has reported.
    #[test]
    fn render_scope_pending_after_the_hold_keeps_the_root_list() {
        use crate::tui::herdr::{HerdrUpdate, Scope};
        let mut app = App::new();
        app.herdr.scoped = true;
        app.herdr.scope_pending = true;
        app.sync_roots(vec![meta("alpha"), meta("beta"), meta("notes")]);
        app.start_loading();
        for (name, rows) in [("alpha", 4), ("beta", 3), ("notes", 0)] {
            app.apply(lastcall_engine::watcher::EngineEvent::Scanned {
                root: root(name),
                rows,
            });
        }
        assert!(
            app.loading.as_ref().is_some_and(|l| l.scanned),
            "the scans are done; only the scope verdict is outstanding"
        );
        assert_eq!(app.listed_roots().count(), 0, "still held back");

        // Below one second: exactly the hold's static frame, so a fast launch that has to
        // wait for the verdict still shows one calm frame.
        let (frame, _) = frame_of(&app, 100, 12);
        assert!(
            frame.contains("discovered 3 repos, checking status…"),
            "{frame}"
        );
        assert!(frame.contains("  alpha  main"), "{frame}");
        assert!(!frame.contains("checked"), "no digits yet: {frame}");
        assert!(
            !frame.contains(SCOPE_PENDING),
            "nor a second screen: {frame}"
        );

        // From one second: the same counter with the holding clause, and every root ticked.
        app.handle(Action::Tick);
        let (frame, _) = frame_of(&app, 100, 12);
        assert!(
            frame.contains(&format!(
                "3 of 3 checked · 7 files pending · {SCOPE_PENDING}"
            )),
            "{frame}"
        );
        assert!(
            !frame.contains("so far"),
            "the scans are not still running: {frame}"
        );
        for row in ["✓ alpha  main", "✓ beta  main", "✓ notes  draft"] {
            assert!(frame.contains(row), "{row}: {frame}");
        }
        assert!(
            frame.contains("lastcall  3 repos · checking status…"),
            "the header keeps the hold's own left half (D3): {frame}"
        );

        // The verdict lists: the hold is over in the same step.
        app.handle(Action::Herdr(HerdrUpdate::Scope(Some(Scope {
            label: "w1".to_owned(),
            roots: [root("alpha"), root("beta"), root("notes")]
                .into_iter()
                .collect(),
        }))));
        assert!(app.loading.is_none(), "{:?}", app.loading);
        let (frame, _) = frame_of(&app, 100, 12);
        assert!(!frame.contains("checking status"), "{frame}");
        // `Scanned` is a tick, not a pile, so the listing's own counts are zero here; the
        // point is that the header is back to counting what is listed.
        assert!(
            frame.contains("lastcall  3 repos · 0 files · 0 hunks"),
            "{frame}"
        );
    }

    /// Verifier (a) F5 and F6. F5: the sponsor's most common launch is "everything is
    /// clean", and since v1.9 that frame lists three empty repo rows with nothing selected
    /// — where `select a file (↑↓ or click)` invited choosing a file that does not exist.
    /// F6: a scope that hides only *empty* repos used to fall through the scoped arm's
    /// `scoped_out() >= 1` gate to the global text, which then listed the very repos the
    /// scope was hiding.
    #[test]
    fn render_all_clean_frame_is_the_empty_state_not_a_prompt() {
        use crate::tui::herdr::{HerdrUpdate, Scope};
        let mut app = three_roots();
        for name in ["alpha", "beta", "notes"] {
            app.apply(pile_event_seq(
                name,
                1,
                lastcall_engine::scan::Pile::default(),
            ));
        }
        assert_eq!(app.listed_roots().count(), 3, "every repo stays listed");
        assert_eq!(app.selection, None, "and none of them is selected");
        let (frame, _) = frame_of(&app, 100, 12);
        assert!(frame.contains("nothing pending across 3 repos"), "{frame}");
        assert!(!frame.contains(NO_SELECTION), "{frame}");

        app.herdr.scoped = true;
        app.handle(Action::Herdr(HerdrUpdate::Scope(Some(Scope {
            label: "w1".to_owned(),
            roots: [root("beta")].into_iter().collect(),
        }))));
        let (frame, _) = frame_of(&app, 100, 12);
        assert!(frame.contains("nothing pending in w1"), "{frame}");
        assert!(frame.contains("2 repos hidden (w shows all)"), "{frame}");
        assert!(
            !frame.contains("nothing pending across"),
            "never the global text under a scope: {frame}"
        );
        assert!(
            !frame.contains("  alpha  ") && !frame.contains("  notes  "),
            "the repos the scope hides are not listed: {frame}"
        );
    }

    /// Gate 8 sponsor run: under a scope whose roots have nothing pending, "nothing pending
    /// across 4 roots" was a lie — three of them had piles the scope was hiding. The empty
    /// state names the scope, lists what it covers, and counts what it hides; while the
    /// verdict is still pending it says that instead.
    #[test]
    fn render_empty_state_under_a_scope_names_it_and_counts_the_hidden() {
        use crate::tui::herdr::{HerdrUpdate, Scope};
        let mut app = three_roots();
        app.sync_roots(vec![
            meta("alpha"),
            meta("beta"),
            meta("notes"),
            meta("quiet"),
        ]);
        // Since Amendment v1.9 every repo in scope is listed, so the empty state under a
        // scope is reached only once `t` has taken the empty ones off too — the two
        // filters stacking, which is exactly the frame this test is about.
        app.hide_empty = true;
        app.herdr.scoped = true;
        app.handle(Action::Herdr(HerdrUpdate::Scope(Some(Scope {
            label: "w2".to_owned(),
            roots: [root("quiet")].into_iter().collect(),
        }))));
        assert_eq!(app.listed_roots().count(), 0);
        let (frame, _) = frame_of(&app, 100, 12);
        assert!(frame.contains("nothing pending in w2"), "{frame}");
        assert!(
            frame.contains("  quiet  "),
            "the in-scope root is listed: {frame}"
        );
        assert!(
            !frame.contains("  alpha  "),
            "the hidden roots are not: {frame}"
        );
        assert!(frame.contains("3 repos hidden (w shows all)"), "{frame}");
        assert!(!frame.contains("nothing pending across"), "{frame}");

        // `w` shows all: the ordinary listing, no empty state at all.
        app.handle(Action::ScopeToggle);
        let (frame, _) = frame_of(&app, 100, 12);
        assert!(frame.contains(NO_SELECTION), "{frame}");

        // Before the first verdict nothing is listed and the pane says why.
        app.handle(Action::ScopeToggle);
        app.herdr.scope_pending = true;
        let (frame, _) = frame_of(&app, 100, 12);
        assert!(frame.contains(SCOPE_PENDING), "{frame}");
        assert!(!frame.contains("nothing pending"), "{frame}");
    }

    /// Deliverable 8 / ruling 1: the scope notice is mandatory *while the scope is
    /// active*, not merely while the status line happens to be free. A transient status —
    /// one is set at startup, after every accept, on a HEAD change and on a focus verdict,
    /// and lives 30 s — shares the row with it: status left, notice right, the status text
    /// truncated first. Below the notice's own width the notice takes the row alone.
    #[test]
    fn render_scope_notice_survives_a_transient_status() {
        use crate::tui::herdr::{HerdrUpdate, Scope};
        let mut app = three_roots();
        app.herdr.scoped = true;
        app.handle(Action::Herdr(HerdrUpdate::Scope(Some(Scope {
            label: "alpha".to_owned(),
            roots: [root("alpha")].into_iter().collect(),
        }))));
        let notice = app.scope_notice().expect("a scope is active");
        assert_eq!(notice, "scope: alpha · 2 repos hidden (w shows all)");

        // With no status the notice sits beside the hints (the pre-existing layout).
        let (frame, _) = frame_of(&app, 100, 12);
        assert!(frame.contains("repos hidden"), "{frame}");

        // With one set it is still there — this is what the old code dropped.
        app.set_status("accepted f1 in alpha");
        let last = |frame: &str| frame.lines().last().unwrap().trim_matches('"').to_owned();
        let (frame, _) = frame_of(&app, 100, 12);
        let row = last(&frame);
        assert!(row.contains("repos hidden"), "{row}");
        assert!(row.starts_with("accepted f1 in alpha · 0s"), "{row}");
        assert!(row.trim_end().ends_with(&notice), "{row}");

        // A long status yields the room rather than pushing the notice off the row.
        app.set_status("x".repeat(200));
        let row = last(&frame_of(&app, 100, 12).0);
        assert!(row.trim_end().ends_with(&notice), "{row}");
        assert!(row.contains('…'), "the status is what truncates: {row}");

        // Too narrow for both: the notice keeps the row, the status yields entirely.
        let row = last(&frame_of(&app, 46, 12).0);
        assert_eq!(row.trim_end(), notice);
    }

    /// Review (b) F7: `d` acks a ready episode; on a blocked root it does nothing, so the
    /// hint ladder must not offer it. `g jump` is offered for either — both have a pane.
    #[test]
    fn render_ack_hint_only_where_d_would_do_something() {
        use crate::tui::herdr::{Attention, HerdrUpdate, RootAgents};
        let flagged = |status: Attention| {
            let mut app = three_roots();
            app.handle(Action::Herdr(HerdrUpdate::Connected {
                version: "0.8.2".to_owned(),
                protocol: 21,
            }));
            app.handle(Action::Herdr(HerdrUpdate::Roots(
                [(
                    root("alpha"),
                    RootAgents {
                        status,
                        agents: 1,
                        pane: Some("w1:p1".to_owned()),
                        agent: Some("claude".to_owned()),
                    },
                )]
                .into_iter()
                .collect(),
            )));
            app.select(Some(Selection::Root(root("alpha"))));
            app
        };

        let blocked = flagged(Attention::Blocked);
        assert!(blocked.herdr.flag(&root("alpha")).unwrap().attention());
        assert_eq!(
            blocked.clone().handle(Action::Ack),
            (Changed::No, None),
            "`d` on a blocked root does nothing"
        );
        let line = hints(&blocked, 200);
        assert!(!line.contains("d ack"), "{line}");
        assert!(line.contains("g jump"), "{line}");

        // A ready episode is what `d` is for — offered before and after the ack, because
        // the flag stays on screen and the key stays the way to explain it.
        let mut done = flagged(Attention::Done);
        assert!(hints(&done, 200).contains("d ack"), "{}", hints(&done, 200));
        assert_eq!(done.handle(Action::Ack).0, Changed::Yes);
        let line = hints(&done, 200);
        assert!(line.contains("d ack"), "{line}");
        assert!(line.contains("g jump"), "{line}");
    }

    /// Review (b) F9: at the nav's real width the flag-only line truncated to
    /// `nothing pending · agent`, losing herdr's status word — the only thing on screen
    /// saying why the root is listed. Narrow rows drop the leading half instead.
    #[test]
    fn render_flag_only_row_keeps_the_status_word_when_it_cannot_fit() {
        use crate::tui::herdr::{Attention, HerdrUpdate, RootAgents};
        let mut app = three_roots();
        app.apply(pile_event_seq(
            "alpha",
            1,
            without(pile("alpha"), &["f1", "f2", "src/parse.rs"]),
        ));
        app.handle(Action::Herdr(HerdrUpdate::Connected {
            version: "0.8.2".to_owned(),
            protocol: 21,
        }));
        app.handle(Action::Herdr(HerdrUpdate::Roots(
            [(
                root("alpha"),
                RootAgents {
                    status: Attention::Done,
                    agents: 1,
                    pane: Some("w1:p1".to_owned()),
                    agent: Some("claude".to_owned()),
                },
            )]
            .into_iter()
            .collect(),
        )));
        app.select(Some(Selection::Root(root("alpha"))));
        // The nav column at 100×30 is 26 wide: `  nothing pending · agent done` is 30.
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(
            frame.lines().any(|l| l.starts_with("\"│  agent done")),
            "the nav keeps herdr's word: {frame}"
        );
        assert!(
            !frame.contains("│  nothing pending · agent"),
            "never the half that says nothing: {frame}"
        );
        // The right pane names the repo now (§6.7, Amendment v1.9) and folds herdr's word
        // into the same sentence, so the status survives there too.
        assert!(
            frame.contains(&nothing_pending_in("alpha", Some("done"))),
            "{frame}"
        );

        // A nav column dragged wide enough keeps the full line.
        app.nav_width = 40;
        let (wide, _) = frame_of(&app, 100, 30);
        assert!(
            wide.contains(&format!("│  {}", nothing_pending("done"))),
            "{wide}"
        );
    }

    /// Ruling R2: a repo with nothing pending is a name-and-branch row on the nav — dim
    /// from the name down, no file rows under it — and its pane names it rather than
    /// repeating the header (§6.7, Amendment v1.9 items 1 and 4).
    #[test]
    fn render_empty_root_row_is_dim_and_has_no_file_rows() {
        let mut app = three_roots();
        app.apply(pile_event_seq(
            "beta",
            1,
            lastcall_engine::scan::Pile::default(),
        ));
        app.select(Some(Selection::Root(root("beta"))));
        let (frame, styles) = frame_of(&app, 100, 30);

        // The nav block for beta is exactly two lines: the name and the branch.
        let nav: Vec<&str> = frame
            .lines()
            .filter(|l| l.starts_with("\"\u{2502}"))
            .map(|l| l["\"\u{2502}".len()..].trim_end_matches(['"', ' ']))
            .collect();
        let at = nav
            .iter()
            .position(|l| l.starts_with("beta"))
            .unwrap_or_else(|| panic!("beta on the nav: {frame}"));
        assert!(nav[at].starts_with("beta"), "{frame}");
        assert!(nav[at + 1].starts_with("  main · 0 files"), "{frame}");
        assert!(
            nav[at + 2].starts_with('\u{2500}') || nav[at + 2].starts_with("notes"),
            "no file rows under an empty repo: {:?}",
            &nav[at..at + 3]
        );
        assert!(
            !frame.contains("nothing pending · agent"),
            "no agent line without an agent: {frame}"
        );

        // beta's name row is bold-dim and its branch line dim; alpha's name row, which has
        // rows under it, is plain bold. The nav's first line is frame row 2 (header, then
        // the pane's top border).
        let mods = |y: usize| -> Vec<String> {
            styles
                .lines()
                .filter(|l| l.starts_with(&format!("{y} ")))
                .map(|l| l.rsplit(' ').next().unwrap_or_default().to_owned())
                .collect()
        };
        let beta_y = 2 + at;
        assert!(
            mods(beta_y)
                .iter()
                .any(|m| m.contains("BOLD") && m.contains("DIM")),
            "beta's name row is bold-dim: {styles}"
        );
        assert!(
            mods(beta_y + 1).iter().any(|m| m.contains("DIM")),
            "its branch line is dim: {styles}"
        );
        assert!(
            mods(2).iter().any(|m| m == "BOLD") && !mods(2).iter().any(|m| m.contains("DIM")),
            "alpha still has rows, so its name row is plain bold: {styles}"
        );

        // The right pane.
        assert!(frame.contains("nothing pending in beta"), "{frame}");
        assert!(
            frame
                .lines()
                .any(|l| l.contains("nothing pending in beta") && !l.contains("· agent")),
            "no agent clause without an agent: {frame}"
        );
    }

    /// The `t` hint says what the key will do, not what the setting is called
    /// (§6.7, Amendment v1.9 item 4; the sponsor's "a simple little show/hide repos").
    #[test]
    fn render_hint_line_names_the_toggle_by_state() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        assert!(hints(&app, 200).contains("t hide empty"), "showing all");
        app.handle(Action::HideEmpty);
        assert!(hints(&app, 200).contains("t show empty"), "hiding");
        assert!(!hints(&app, 200).contains("t hide empty"));
        // A rebound key is named by its own spec, like every other hint.
        for (name, specs) in &mut app.keymap {
            if name == "hide_empty" {
                *specs = vec!["ctrl-t".to_owned()];
            }
        }
        assert!(
            hints(&app, 200).contains("^T show empty"),
            "{}",
            hints(&app, 200)
        );
    }

    #[test]
    fn render_too_small_is_one_line() {
        let app = App::new();
        let (frame, styles) = frame_of(&app, 30, 8);
        assert!(frame.contains("too small: 40×10 min"), "{frame}");
        assert_eq!(styles, "");
    }

    #[test]
    fn render_narrow_hides_nav_and_highlights_main() {
        let app = App::new();
        let (frame, _) = frame_of(&app, 69, 12);
        assert!(!frame.contains("┬"), "{frame}");
        let (frame, _) = frame_of(&app, 70, 12);
        assert!(frame.contains("┬"), "{frame}");
    }

    /// At 60 columns the header cannot hold both `[Accept All]` and `watching W`: the
    /// notice wins (nothing else says what is watched; `^A` duplicates the control), and
    /// the control's target goes with it. At 100 both are there.
    #[test]
    fn render_narrow_header_keeps_the_notice_and_drops_the_control() {
        let app = three_roots();
        let header_and_hits = |w: u16| {
            let mut terminal = Terminal::new(TestBackend::new(w, 20)).unwrap();
            let mut hits = HitMap::default();
            terminal
                .draw(|f| {
                    hits = render(&app, f);
                })
                .unwrap();
            let frame = terminal.backend().to_string();
            let header = frame.lines().next().unwrap().trim_matches('"').to_owned();
            let control = hits
                .targets
                .iter()
                .any(|(_, t)| *t == Target::HeaderAcceptAll);
            (header, control)
        };
        let (header, control) = header_and_hits(60);
        assert!(!header.contains("[Accept All]"), "{header}");
        assert!(header.trim_end().ends_with("watching W"), "{header}");
        assert!(!control, "no control, no target");
        let (header, control) = header_and_hits(100);
        assert!(header.contains("  [Accept All]"), "{header}");
        assert!(header.trim_end().ends_with("watching W"), "{header}");
        assert!(control);
    }

    #[test]
    fn render_hit_map_prefers_specific_targets() {
        let mut hits = HitMap::default();
        hits.targets
            .push((Rect::new(0, 0, 10, 10), Target::DiffBody));
        hits.targets
            .push((Rect::new(0, 3, 10, 1), Target::DiffHunk(2)));
        assert_eq!(hits.at(5, 3), Some(&Target::DiffHunk(2)));
        assert_eq!(hits.at(5, 4), Some(&Target::DiffBody));
        assert_eq!(hits.at(50, 4), None);
        hits.nav = Some(Rect::new(0, 0, 4, 10));
        hits.main = Some(Rect::new(4, 0, 6, 10));
        assert_eq!(hits.pane_at(1, 1), Some(Pane::Nav));
        assert_eq!(hits.pane_at(5, 1), Some(Pane::Diff));
        assert_eq!(hits.pane_at(50, 1), None);
    }

    #[test]
    fn render_styles_reports_runs() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 2));
        buf.set_string(1, 0, "ab", bold());
        buf.set_string(3, 0, "c", bold().add_modifier(Modifier::DIM));
        buf.set_string(0, 1, "x", green());
        assert_eq!(
            styles(&buf),
            "0 1..3 Reset Reset BOLD\n0 3..4 Reset Reset BOLD|DIM\n1 0..1 Green Reset -\n"
        );
    }

    /// Amendment v1.11: `z undo` is offered only where `z` would do something — the same
    /// rule as `d ack` and `A accept all in <root>`. The pile's own `undo` count is what
    /// decides, so a second process's accept puts the hint up at the next pile.
    #[test]
    fn render_hint_line_offers_z_undo_only_when_there_is_something_to_undo() {
        let mut app = three_roots();
        app.handle(Action::Resize(120, 30));
        app.select(Some(row("alpha", "f1")));
        assert!(
            !hints(&app, 120).contains("z undo"),
            "an empty stack promises nothing: {}",
            hints(&app, 120)
        );

        app.apply(pile_event_seq("alpha", 1, undo_pile(pile("alpha"), 1)));
        let line = hints(&app, 120);
        assert!(line.contains("z undo"), "{line}");
        // beta's stack is its own: the hint follows the selection, not the frame.
        app.select(Some(row("beta", "u1")));
        assert!(!hints(&app, 120).contains("z undo"), "{}", hints(&app, 120));

        // It sits between `t hide/show empty` and the herdr jumps in the drop order, so a
        // line that has dropped down to the accept phrases has dropped it too.
        app.select(Some(row("alpha", "f1")));
        let narrow = hints(&app, 60);
        assert!(!narrow.contains("z undo"), "{narrow}");
        assert!(narrow.contains("? help"), "the pinned pair stays: {narrow}");
    }

    /// The sponsor's own run (2026-09-14): the wake was not apparent. `s` is offered on a
    /// repository row only, and its label says which of its two jobs it will do there.
    #[test]
    fn hints_offer_snooze_on_a_repo_row_and_wake_on_a_snoozed_one() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        assert!(
            !hints(&app, 200).contains("s snooze") && !hints(&app, 200).contains("s wake"),
            "a file row does not offer s: {}",
            hints(&app, 200)
        );
        app.select(Some(Selection::Root(root("beta"))));
        assert!(
            hints(&app, 200).contains("s snooze"),
            "{}",
            hints(&app, 200)
        );
        app.handle(Action::ShowSnoozed);
        app.apply(pile_event_seq(
            "beta",
            1,
            snoozed_pile(pile("beta"), "2026-09-20T09:00:00Z"),
        ));
        app.select(Some(Selection::Root(root("beta"))));
        let line = hints(&app, 200);
        assert!(
            line.contains("s wake") && !line.contains("s snooze"),
            "{line}"
        );
    }

    /// The repository row's branch line carries the deadline, dimmed: the nav is where a
    /// reader looks to ask why a repository is quiet, and `shift-s` is the only way it is
    /// on screen at all.
    #[test]
    fn render_nav_says_when_a_repo_is_snoozed() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        // The suffix rides the branch line, so it wants a nav pane wide enough to hold it;
        // a narrower one clips it like any other branch-line part.
        app.nav_width = 50;
        app.handle(Action::ShowSnoozed);
        app.apply(pile_event_seq(
            "beta",
            1,
            snoozed_pile(pile("beta"), "2026-09-20T09:00:00Z"),
        ));
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(
            frame.contains("snoozed until 2026-09-20"),
            "the date, not the timestamp: {frame}"
        );
        assert!(
            !frame.contains("09:00:00"),
            "the time of day is noise here: {frame}"
        );
    }

    /// R5: the pane for a selected root says where that root is, with the home folder
    /// written `~`. A name is a folder name two repositories can share, and a watched
    /// folder's name is one the configuration chose, so the path is the line that answers
    /// "which one is this?".
    #[test]
    fn render_a_selected_root_shows_where_it_is() {
        let mut app = three_roots();
        app.handle(Action::Resize(120, 30));
        app.select(Some(Selection::Root(root("notes"))));
        let (frame, _) = frame_of(&app, 120, 30);
        assert!(frame.contains("~/W/notes"), "the path line: {frame}");
        // And for a root with nothing pending, whose first line is the empty sentence.
        app.apply(pile_event_seq(
            "notes",
            2,
            lastcall_engine::scan::Pile::default(),
        ));
        let (frame, _) = frame_of(&app, 120, 30);
        assert!(
            frame.contains("nothing pending in notes") && frame.contains("~/W/notes"),
            "an empty root shows its path too: {frame}"
        );
    }

    /// A watched folder's name can carry the folders above it, so a name wider than the nav
    /// column is cut at the head: the tail is the folder itself.
    #[test]
    fn render_nav_cuts_a_long_root_name_at_the_head() {
        let mut app = three_roots();
        app.handle(Action::Resize(120, 30));
        app.nav_width = 20;
        let long = "a-very-long-parent-folder/notes";
        if let Some(view) = app.roots.get_mut(&root("notes")) {
            view.meta.name = long.to_owned();
        }
        let (frame, _) = frame_of(&app, 120, 30);
        assert!(
            frame.contains("…"),
            "a name wider than the column is cut: {frame}"
        );
        assert!(
            !frame.contains("a-very-long-parent-folder/notes"),
            "the whole name does not fit, so it is not there: {frame}"
        );
        assert!(
            frame.contains("notes"),
            "the tail survives the cut: {frame}"
        );
    }

    /// The modal names the repository it is about and shows the digits as typed, with the
    /// caret the note modal uses. Its own keys are on the last row, because the hint line
    /// belongs to the nav.
    #[test]
    fn render_snooze_modal_names_the_repo_and_the_day_field() {
        let mut app = three_roots();
        app.handle(Action::Resize(100, 30));
        app.select(Some(Selection::Root(root("beta"))));
        app.handle(Action::Snooze);
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(frame.contains("snooze beta for [1"), "{frame}");
        assert!(frame.contains("day(s)"), "{frame}");
        assert!(frame.contains(SNOOZE_KEYS), "{frame}");
        assert!(frame.contains(" snooze "), "the box is titled: {frame}");

        app.handle(Action::SnoozeEdit(SnoozeKey::Backspace));
        app.handle(Action::SnoozeEdit(SnoozeKey::Digit('3')));
        app.handle(Action::SnoozeEdit(SnoozeKey::Digit('0')));
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(frame.contains("snooze beta for [30"), "{frame}");

        // Esc takes it off the frame entirely.
        app.handle(Action::SnoozeEdit(SnoozeKey::Cancel));
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(!frame.contains("day(s)"), "{frame}");
    }

    // --- word wrap in the diff pane (Phase 13, deliverable A) ----------------------------

    /// Draw `app` and hand back the buffer and the hit map, so a wrap test can ask both
    /// what was drawn and which line each row belongs to.
    fn drawn(app: &App, w: u16, h: u16) -> (Buffer, HitMap) {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut hits = HitMap::default();
        terminal.draw(|f| hits = render(app, f)).unwrap();
        (terminal.backend().buffer().clone(), hits)
    }

    /// An app on `f1` whose single hunk's body is `lines`, diff focused.
    fn wrapped(lines: &[&str]) -> App {
        let mut app = three_roots();
        app.apply(pile_event("alpha", alpha_texts(lines)));
        app.select(Some(row("alpha", "f1")));
        app.handle(Action::Open);
        app.handle(Action::Resize(100, 30));
        app
    }

    /// The symbols of row `y` across `rect`, wide characters included (their second cell
    /// carries an empty symbol, so a plain concatenation is the text).
    fn row_text(buf: &Buffer, rect: Rect, y: u16) -> String {
        (rect.x..rect.right())
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    /// The rows the hit map says belong to diff line `line`.
    fn rows_of(hits: &HitMap, line: usize) -> Vec<u16> {
        let body = hits.diff_body.expect("the diff body");
        hits.diff_rows
            .iter()
            .enumerate()
            .filter(|(_, l)| **l == line)
            .map(|(i, _)| body.y + i as u16)
            .collect()
    }

    /// A continuation row is still the line it continues: the same gutter mark and the
    /// same colour, so a wrapped insertion (or deletion: code verification F6, M3) does not
    /// turn into context half way down. Only the continuation's gutter mark is dim (F5), so
    /// one wrapped `+` line does not read as several.
    #[test]
    fn render_wrap_continuation_rows_keep_the_gutter_and_the_colour() {
        for (tag, mark, colour) in [
            (Tag::Insert, '+', Color::Green),
            (Tag::Delete, '-', Color::Red),
        ] {
            let mut app = wrapped(&["a"]);
            // One long changed line, the only body line of the hunk.
            let hunk = &mut app.selected_row().expect("f1").hunks[0].clone();
            hunk.lines = vec![(tag, format!("{}\n", "lorem ipsum ".repeat(20)).into_bytes())];
            let mut p = pile("alpha");
            p.rows[0].hunks = vec![hunk.clone()];
            app.apply(pile_event("alpha", p));
            app.select(Some(row("alpha", "f1")));
            app.handle(Action::Open);

            let (buf, hits) = drawn(&app, 100, 30);
            let body = hits.diff_body.expect("the body");
            let rows = rows_of(&hits, 1);
            assert!(rows.len() > 2, "the line wrapped: {rows:?}");
            for (i, y) in rows.into_iter().enumerate() {
                let text = row_text(&buf, body, y);
                assert!(text.starts_with(mark), "row {y}: {text:?}");
                assert_eq!(buf[(body.x, y)].fg, colour, "row {y} is a {tag:?}");
                assert_eq!(buf[(body.x + 1, y)].fg, colour, "and so is its text");
                assert_eq!(
                    buf[(body.x, y)].modifier.contains(Modifier::DIM),
                    i > 0,
                    "row {y}: only a continuation's gutter mark is dim"
                );
                assert!(
                    !buf[(body.x + 1, y)].modifier.contains(Modifier::DIM),
                    "row {y}: the text is never dim"
                );
            }
        }
    }

    /// Code verification F4: before a frame has reported the body's size (the first key,
    /// and every key folded into a batch with a resize) the reducer lays out against a
    /// fallback, and it has to be the body the renderer then draws, at every size and nav
    /// width, or a page could step past a row that was never drawn.
    #[test]
    fn render_diff_body_fallback_is_what_the_renderer_draws() {
        for (w, h) in [
            (40u16, 10u16),
            (59, 20),
            (60, 20),
            (80, 24),
            (100, 30),
            (120, 40),
            (200, 50),
        ] {
            for nav in [10u16, 28, 40, 90, 300] {
                let mut app = wrapped(&["short", &"x".repeat(400), "tail"]);
                app.nav_width = nav;
                app.handle(Action::Resize(w, h));
                assert_eq!(app.diff_size, None, "a resize forgets the measure");
                let fallback = app.diff_body_size();
                let (_, hits) = drawn(&app, w, h);
                let real = hits.diff_body.map(|r| (r.width, r.height));
                assert_eq!(real, Some(fallback), "{w}x{h} nav_width={nav}");
            }
        }
    }

    /// Re-verification R3: a root's notices take rows off the top of the body, and they are
    /// still there after a resize. The fallback keeps the last frame's shortfall, so a page
    /// key folded into the same pass as the resize pages by the body that will be drawn
    /// (the probe's input: three notices at 100x30, fallback 25 rows over a drawn 22).
    #[test]
    fn render_diff_body_fallback_is_short_by_what_the_last_frame_stacked_above_it() {
        let lines: Vec<String> = (0..60).map(|i| format!("line {i}")).collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        for wrap in [true, false] {
            let mut app = wrapped(&lines);
            app.wrap = wrap;
            app.roots.get_mut(&root("alpha")).unwrap().pile.notices =
                (0..3).map(|i| format!("notice {i}")).collect();
            let (_, hits) = drawn(&app, 100, 30);
            let real = hits.diff_body.map(|r| (r.width, r.height));
            assert_eq!(real, Some((71, 22)));
            app.measured_diff_body(real);
            app.handle(Action::Resize(100, 30));
            assert_eq!(app.diff_size, None);
            assert_eq!(Some(app.diff_body_size()), real, "wrap {wrap}");
            app.focus = Focus::Diff;
            app.handle(Action::NavPageDown);
            assert!(
                app.diff.scroll <= 22,
                "wrap {wrap}: no line is passed undrawn, top {}",
                app.diff.scroll
            );
        }
    }

    /// F12: every character of a line reaches the buffer, whatever it is made of. The
    /// renderer clips at the pane's width, so a row measured wrongly loses its tail
    /// silently; this is the test that would catch it.
    #[test]
    fn render_wrap_puts_every_character_of_a_line_on_the_screen() {
        let text = concat!(
            "\u{4F60}\u{597D}\u{4E16}\u{754C} ",
            "\u{26A0}\u{FE0F} warning ",
            "e\u{0301}te\u{0301} ",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} family ",
            "\u{65E5}\u{672C}\u{8A9E}\u{306E}\u{30C6}\u{30AD}\u{30B9}\u{30C8}",
        );
        let text = &text.repeat(4);
        let app = wrapped(&[text]);
        let (buf, hits) = drawn(&app, 100, 30);
        let body = hits.diff_body.expect("the body");
        let rows = rows_of(&hits, 1);
        assert!(rows.len() > 1, "it wrapped");
        let mut drawn_text = String::new();
        for y in rows {
            let row = row_text(&buf, body, y);
            let row = row
                .strip_prefix(' ')
                .expect("the context gutter")
                .to_owned();
            drawn_text.push_str(row.trim_end());
        }
        // Trailing whitespace is what a row break ate; everything else is there, in order.
        let want: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let got: String = drawn_text.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(got, want, "drew {drawn_text:?}");
    }

    /// The selection band is a statement about lines, so it covers every row of one.
    #[test]
    fn render_wrap_the_selection_band_covers_every_row_of_a_line() {
        let mut app = wrapped(&["short", &"x".repeat(400)]);
        app.handle(Action::NavDown);
        app.handle(Action::NavDown);
        app.handle(Action::Select);
        let (buf, hits) = drawn(&app, 100, 30);
        let body = hits.diff_body.expect("the body");
        let rows = rows_of(&hits, 2);
        assert!(rows.len() > 1, "the selected line wrapped: {rows:?}");
        for y in &rows {
            for x in body.x..body.right() {
                assert!(
                    buf[(x, *y)].modifier.contains(Modifier::REVERSED),
                    "({x}, {y}) is outside the band"
                );
            }
        }
        // And it stops at the line's last row.
        let after = rows[rows.len() - 1] + 1;
        if after < body.bottom() {
            assert!(!buf[(body.x, after)].modifier.contains(Modifier::REVERSED));
        }
    }

    /// The header's own row still carries its controls, with a wrapped line underneath.
    #[test]
    fn render_wrap_a_header_keeps_its_controls_over_a_wrapped_line() {
        let app = wrapped(&[&"x".repeat(400)]);
        let (buf, hits) = drawn(&app, 100, 30);
        let body = hits.diff_body.expect("the body");
        assert_eq!(hits.diff_rows[0], 0, "the header is the first row");
        let header = row_text(&buf, body, body.y);
        assert!(header.contains("@@ -"), "{header:?}");
        assert!(
            header.contains("accept"),
            "the controls are there: {header:?}"
        );
        assert!(rows_of(&hits, 1).len() > 1, "and the line below it wrapped");
    }

    /// A line the cap cut ends in a dim marker naming how much is not shown.
    #[test]
    fn render_wrap_the_caps_marker_is_dim_and_counts() {
        // A body line far taller than the pane: 4000 characters in a pane under 100 wide.
        let app = wrapped(&["x".repeat(4000).as_str()]);
        let (buf, hits) = drawn(&app, 100, 30);
        let body = hits.diff_body.expect("the body");
        let rows = rows_of(&hits, 1);
        let last = *rows.last().expect("rows");
        let text = row_text(&buf, body, last);
        let at = text
            .find(" … +")
            .unwrap_or_else(|| panic!("a marker in {text:?}"));
        let marker_x = body.x + at as u16;
        assert!(
            buf[(marker_x + 1, last)].modifier.contains(Modifier::DIM),
            "the marker is dim: {}",
            styles(&buf)
        );
        assert!(
            !buf[(body.x + 1, last)].modifier.contains(Modifier::DIM),
            "but the text it follows is not"
        );
        // The cap leaves room for what comes after the line.
        assert!(
            rows.len() < body.height as usize,
            "{} rows of {}",
            rows.len(),
            body.height
        );
    }

    /// With wrap off the row table is the pre-phase one: row = line, from the scroll.
    #[test]
    fn render_wrap_off_gives_one_row_per_line() {
        let mut app = wrapped(&["short", &"x".repeat(400), "tail"]);
        app.handle(Action::ToggleWrap);
        assert!(!app.wrap);
        let (buf, hits) = drawn(&app, 100, 30);
        let body = hits.diff_body.expect("the body");
        assert_eq!(
            hits.diff_rows[..4],
            [0, 1, 2, 3],
            "one row per line, in order"
        );
        let long = row_text(&buf, body, body.y + 2);
        assert!(long.starts_with(" xxx"), "clipped, not wrapped: {long:?}");
        assert_eq!(row_text(&buf, body, body.y + 3).trim(), "tail");
    }

    /// Read row `y` of `buf` as the symbols actually drawn.
    fn drawn_row(buf: &Buffer, w: u16, y: u16) -> String {
        (0..w).map(|x| buf[(x, y)].symbol().to_owned()).collect()
    }

    /// Code verification F1, the product's promise at the level it is made: with wrap on
    /// and no cap in play, **every character of a line is on some row of the buffer**, for
    /// the scripts where a string's width and the cells ratatui draws disagree (an Arabic
    /// lam and alef, a halfwidth dakuten), and for everything else a row can end on. Drawn
    /// through the real `render_hunks` into a buffer exactly as wide as the body, over all
    /// three tags; the gutter's colour is checked on every row on the way.
    ///
    /// Control characters are left out of the comparison because ratatui draws them
    /// nowhere, wrapped or not; whitespace and the two invisible format characters are left
    /// out because a cell a wide symbol covers reads back as a space.
    #[test]
    fn render_wrap_every_character_is_on_some_row_in_every_script() {
        let inputs: Vec<(&str, String)> = vec![
            ("halfwidth-dakuten", "\u{FF76}\u{FF9E}\u{FF77}\u{FF9E}\u{FF78}\u{FF9E}".repeat(20)),
            ("arabic-lam-alef", "\u{0644}\u{0627}".repeat(40)),
            ("arabic-words", "\u{0644}\u{0627} \u{0633}\u{0644}\u{0627}\u{0645} ".repeat(15)),
            ("vs16", "\u{26A0}\u{FE0F}".repeat(40)),
            ("zwj-family", "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}".repeat(30)),
            ("flags", "\u{1F1FA}\u{1F1F8}\u{1F1EC}\u{1F1E7}".repeat(20)),
            ("flags-odd", format!("a{}", "\u{1F1FA}".repeat(41))),
            ("cjk-edge-odd", format!("a{}", "\u{4E16}".repeat(60))),
            ("cjk-edge-even", "\u{4E16}".repeat(60)),
            ("combining", "e\u{0301}".repeat(80)),
            ("leading-combining", format!("\u{0301}{}", "abc ".repeat(30))),
            ("leading-dakuten", format!("\u{FF9E}{}", "abc ".repeat(30))),
            ("nbsp", "a\u{00A0}".repeat(60)),
            ("ideographic-space", "\u{3000}".repeat(60)),
            ("em-space-words", "word\u{2003}".repeat(30)),
            ("trailing-ws", format!("abc{}", " ".repeat(150))),
            ("only-spaces", " ".repeat(150)),
            ("tabs", "\tx\t\ty".repeat(20)),
            ("zwsp", "ab\u{200B}".repeat(50)),
            ("skin-tone", "\u{1F44D}\u{1F3FD}".repeat(40)),
            ("keycap", "1\u{FE0F}\u{20E3}".repeat(40)),
            ("hangul-jamo", "\u{1100}\u{1161}\u{11A8}".repeat(40)),
            ("devanagari", "\u{0915}\u{094D}\u{0937}\u{093F} ".repeat(30)),
            ("thai", "\u{0E01}\u{0E33}\u{0E19}\u{0E49}\u{0E33}".repeat(30)),
            ("soft-hyphen", "ab\u{00AD}cd".repeat(40)),
            ("control", "ab\u{1b}[31mcd\u{7}".repeat(20)),
            (
                "mixed",
                "\u{4F60}\u{597D} \u{26A0}\u{FE0F} e\u{0301} \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} \u{FF76}\u{FF9E} \u{0644}\u{0627} x"
                    .repeat(10),
            ),
        ];
        let visible = |s: &str| -> String {
            s.chars()
                .filter(|c| {
                    !c.is_control() && !c.is_whitespace() && *c != '\u{200B}' && *c != '\u{AD}'
                })
                .collect()
        };
        let mut app = three_roots();
        app.wrap = true;
        app.diff.scroll = 1;
        let mut failures: Vec<String> = Vec::new();
        for (name, text) in &inputs {
            for w in [3u16, 4, 5, 7, 10, 11, 19, 20, 37, 63, 80] {
                for tag in [Tag::Context, Tag::Insert, Tag::Delete] {
                    let hunks = vec![Hunk {
                        index: 0,
                        old_range: 0..1,
                        new_range: 0..1,
                        lines: vec![(tag, format!("{text}\n").into_bytes())],
                    }];
                    // Tall enough that the cap is never in play.
                    let area = Rect::new(0, 0, w, 400);
                    let mut buf = Buffer::empty(area);
                    let mut hits = HitMap::default();
                    render_hunks(
                        &app,
                        &mut buf,
                        area,
                        &hunks,
                        &[],
                        HunkControls::All,
                        &mut hits,
                    );
                    let table = wrap::layout(&hunks, 1, w, 400, true);
                    assert!(
                        !table.iter().any(|r| matches!(
                            &r.part,
                            wrap::Part::Slice {
                                marker: Some(_),
                                ..
                            }
                        )),
                        "the premise: {name} at {w} is not capped"
                    );
                    let mut drawn = String::new();
                    for (y, row) in table.iter().enumerate().filter(|(_, r)| r.line == 1) {
                        let got = drawn_row(&buf, w, y as u16);
                        let gutter = wrap::gutter_of(tag);
                        match got.strip_prefix(gutter) {
                            Some(rest) => drawn.push_str(rest),
                            None => {
                                failures.push(format!("{name} w={w} {tag:?} row={y}: no gutter"))
                            }
                        }
                        let fg = buf[(0, y as u16)].fg;
                        let coloured = match tag {
                            Tag::Insert => fg == Color::Green,
                            Tag::Delete => fg == Color::Red,
                            Tag::Context => true,
                        };
                        if !coloured {
                            failures
                                .push(format!("{name} w={w} {tag:?} row={y}: gutter is {fg:?}"));
                        }
                        let _ = row;
                    }
                    let want = visible(&line_text(text.as_bytes()));
                    if visible(&drawn) != want {
                        failures.push(format!(
                            "{name} w={w} {tag:?}: drew {} of {} visible characters",
                            visible(&drawn).chars().count(),
                            want.chars().count()
                        ));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "hidden text:\n{}", failures.join("\n"));
    }
}
