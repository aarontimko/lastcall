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

use lastcall_engine::hunks::{Hunk, Tag};
use lastcall_engine::scan::{Change, Collapsed, Rename, Row};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Widget};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{
    AcceptScope, App, Focus, MIN_SIZE, NAV_MIN_COLS, RootView, Selection, Target, annotation_name,
    diff_len, hunk_offsets, plural,
};
use super::input::{Action, MODAL_KEYS};

pub const TOO_SMALL: &str = "too small: 40×10 min";
pub const NO_SELECTION: &str = "select a file (↑↓ or click) · ? for help";

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
    render_header(app, buf, header, &mut hits);
    render_status(app, buf, status);

    let nav_visible = area.width >= NAV_MIN_COLS;
    let focus = if nav_visible { app.focus } else { Focus::Diff };
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
    render_main(app, buf, main_inner, &mut hits);

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
        hits.targets
            .push((Rect::new(x, body.y, 1, body.height), Target::Divider));
    }

    if app.help {
        render_help(app, buf, area);
    }
    if app.confirm.is_some() {
        render_confirm(app, buf, area);
    }
    hits
}

/// `lastcall  <repos> · <files> · <hunks>  [Accept All]` … `watching <parents>`. The file
/// count carries `+` when any listed root's pile stopped at the row cap; the control is
/// dim when nothing is listed and is the `HeaderAcceptAll` target either way. When the
/// control and the notice do not both fit (60 columns), the control goes and the notice
/// stays: `^A` duplicates the control, nothing else says what is being watched.
fn render_header(app: &App, buf: &mut Buffer, area: Rect, hits: &mut HitMap) {
    let listed: Vec<&RootView> = app.listed_roots().collect();
    let files: usize = listed.iter().map(|v| v.rows().len()).sum();
    let hunks: usize = listed
        .iter()
        .flat_map(|v| v.rows().iter())
        .map(|r| r.hunks.len())
        .sum();
    let left = format!(
        "lastcall  {} · {} · {}",
        plural(listed.len(), "repo"),
        count_plus(files, app.any_truncated(), "file"),
        plural(hunks, "hunk")
    );
    let control = "[Accept All]";
    let control_x = area.x + (left.width() + 2) as u16;
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
    let with_control = left.width() + 2 + control.width();
    // Priority when the line is short: the counts, then the notice, then the control.
    let both_fit = with_control + 2 + right.width() <= width;
    let notice_fits_alone = left.width() + 2 + right.width() <= width;
    let show_control = both_fit || !notice_fits_alone;
    let show_notice = both_fit || notice_fits_alone;
    let mut used = left.width();
    let mut spans = vec![Span::styled(left, bold())];
    if show_control {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            control,
            if listed.is_empty() {
                dim()
            } else {
                Style::new()
            },
        ));
        used = with_control;
    }
    if show_notice {
        let pad = width.saturating_sub(used + right.width());
        spans.push(Span::raw(format!("{}{right}", " ".repeat(pad))));
    }
    buf.set_line(area.x, area.y, &Line::from(spans), area.width);
    if show_control && with_control <= width {
        hits.targets.push((
            Rect::new(control_x, area.y, control.width() as u16, 1),
            Target::HeaderAcceptAll,
        ));
    }
}

/// `plural`, with `+` after the number when the count is a truncated one (`4+ files`).
fn count_plus(n: usize, plus: bool, noun: &str) -> String {
    if plus {
        format!("{n}+ {noun}s")
    } else {
        plural(n, noun)
    }
}

fn render_status(app: &App, buf: &mut Buffer, area: Rect) {
    let line = match (&app.status, app.status_age()) {
        (Some(s), Some(age)) => Line::from(vec![
            Span::raw(s.text.clone()),
            Span::styled(format!(" · {age}"), dim()),
        ]),
        _ => Line::from(Span::styled(hints(app, area.width), dim())),
    };
    buf.set_line(area.x, area.y, &line, area.width);
}

/// The hint line from the app's own keymap: `↑↓ select  ⏎ open  n/p hunk  <accept>  ^A
/// accept all  Tab focus  r refresh  ? help  q quit`, where `<accept>` follows the
/// selection — `a accept hunk  A accept file` on a file row with diff focus, `a/A accept
/// file` on a file row otherwise, `a accept group` on a group entry, `a accept all in
/// <root>` on a root entry (how the per-repo fold is told from the header's global one).
/// Below `NAV_MIN_COLS`, or when the line would not fit, the `focus` and `refresh` hints
/// are dropped. While the confirm modal is open the line is `y confirm  n cancel  q quit`:
/// exactly the keys that work there (the modal's own, fixed, and the keymap's `quit`).
pub fn hints(app: &App, width: u16) -> String {
    let first = |action: &str| app.keys_for(action).first().map(|s| hint_label(s));
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
    let context = match &scope {
        Some(AcceptScope::Hunk { .. }) => accept.as_ref().map(|k| format!("{k} accept hunk")),
        Some(AcceptScope::File { .. }) => match (&accept, &accept_file) {
            (Some(a), Some(f)) => Some(format!("{a}/{f} accept file")),
            (Some(a), None) => Some(format!("{a} accept file")),
            (None, Some(f)) => Some(format!("{f} accept file")),
            (None, None) => None,
        },
        Some(AcceptScope::Group { .. }) => accept.as_ref().map(|k| format!("{k} accept group")),
        Some(AcceptScope::Root(root)) => accept
            .as_ref()
            .map(|k| format!("{k} accept all in {}", app.root_name(root))),
        Some(AcceptScope::All) | None => None,
    };
    let file = match &scope {
        Some(AcceptScope::Hunk { .. }) => accept_file.map(|k| format!("{k} accept file")),
        _ => None,
    };
    // (hint, tier): when the line must shrink, tier 2 goes first (`focus`, `refresh`;
    // always below `NAV_MIN_COLS`), then tier 1 (the file and global accept hints).
    let items = [
        (pair("nav_up", "nav_down").map(|k| format!("{k} select")), 0),
        (first("open").map(|k| format!("{k} open")), 0),
        (
            pair("hunk_next", "hunk_prev").map(|k| format!("{k} hunk")),
            0,
        ),
        (context, 0),
        (file, 1),
        (first("accept_all").map(|k| format!("{k} accept all")), 1),
        (first("focus_toggle").map(|k| format!("{k} focus")), 2),
        (first("refresh").map(|k| format!("{k} refresh")), 2),
        (first("help").map(|k| format!("{k} help")), 0),
        (first("quit").map(|k| format!("{k} quit")), 0),
    ];
    let join = |max_tier: u8| -> String {
        items
            .iter()
            .filter(|(_, tier)| *tier <= max_tier)
            .filter_map(|(hint, _)| hint.clone())
            .collect::<Vec<_>>()
            .join("  ")
    };
    let fits = |s: &str| s.width() <= width as usize;
    let full = join(2);
    if width >= NAV_MIN_COLS && fits(&full) {
        return full;
    }
    let mid = join(1);
    if fits(&mid) { mid } else { join(0) }
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
    let mut first = true;
    for (path, view) in &app.roots {
        if !view.listed() {
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
        let mut spans = vec![Span::styled(view.meta.name.clone(), bold())];
        let remote = view.meta.remote.as_deref().filter(|_| app.show_remote);
        if let Some(remote) = remote {
            let budget = width.saturating_sub(view.meta.name.width() + 2);
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
        lines.push(NavLine {
            line: Line::from(format!(
                "  {} · {}",
                view.meta.branch_label(),
                count_plus(view.rows().len(), view.pile.omitted > 0, "file")
            )),
            target: None,
            selected: false,
        });
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

    let rows = area.height as usize;
    let offset = match selected_at {
        Some(i) if i >= rows => i + 1 - rows,
        _ => 0,
    };
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
    if row.flag.is_some() {
        markers.push_str(" ⚑");
    }
    let conflict = if row.conflicted { "  [conflict]" } else { "" };
    let counts_added = format!("+{}", row.added);
    let counts_deleted = format!("−{}", row.deleted);
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
            if app.listed_roots().next().is_none() {
                lines.push(Line::from(format!(
                    "nothing pending across {}",
                    plural(app.roots.len(), "root")
                )));
                for view in app.roots.values() {
                    let mut text = format!("  {}  {}", view.meta.name, view.meta.branch_label());
                    for label in [view.meta.badge_label(), view.meta.in_progress_label()]
                        .into_iter()
                        .flatten()
                    {
                        text.push_str("  ");
                        text.push_str(&label);
                    }
                    lines.push(Line::from(text));
                }
            } else {
                lines.push(Line::from(Span::styled(NO_SELECTION, dim())));
            }
        }
        Some(Selection::Root(root)) => {
            if let Some(view) = app.roots.get(root) {
                let mut spans = vec![
                    Span::styled(view.meta.name.clone(), bold()),
                    Span::raw(format!(
                        "  {} · {}",
                        view.meta.branch_label(),
                        count_plus(view.rows().len(), view.pile.omitted > 0, "file")
                    )),
                ];
                for label in [view.meta.badge_label(), view.meta.in_progress_label()]
                    .into_iter()
                    .flatten()
                {
                    spans.push(Span::raw(format!("  {label}")));
                }
                lines.push(Line::from(spans));
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
                let mut header = row_header(row);
                let control = format!("[{} accept file]", control_key(app, "accept_file"));
                if let Some(x) = right_align(&mut header, &control, area.width, dim()) {
                    hits.targets.push((
                        Rect::new(area.x + x, area.y, control.width() as u16, 1),
                        Target::FileAccept,
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

fn push_notices(lines: &mut Vec<Line<'static>>, notices: &[String]) {
    for n in notices {
        lines.push(Line::from(Span::styled(n.clone(), dim())));
    }
}

/// `<path>  <letter>  +a −d  [annotation]  (renamed from <old> 90%)`
fn row_header(row: &Row) -> Line<'static> {
    let mut spans = vec![
        Span::styled(row.path_lossy(), bold()),
        Span::raw(format!("  {}  ", letter(row.change))),
        Span::styled(format!("+{}", row.added), green()),
        Span::raw(" "),
        Span::styled(format!("−{}", row.deleted), red()),
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
    if let Some(f) = &row.flag {
        spans.push(Span::raw(format!("  ⚑ {}", f.note)));
    }
    Line::from(spans)
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
            let kind = match kind {
                Collapsed::Glob => "glob",
                Collapsed::Binary => "binary",
                Collapsed::Size => "size",
            };
            let text = format!(
                "collapsed ({kind}) · +{} −{} · expands in a later phase",
                row.added, row.deleted
            );
            buf.set_line(area.x, area.y, &single(text, dim()), area.width);
            return;
        }
        _ => {}
    }
    if row.hunks.is_empty() {
        return;
    }
    let total = diff_len(row);
    let offsets = hunk_offsets(&row.hunks);
    let scroll = app.diff.scroll.min(total.saturating_sub(1));
    let current = app.diff.hunk.min(row.hunks.len() - 1);
    // The hunk containing `scroll`, and the line within it.
    let mut h = offsets.partition_point(|&o| o <= scroll).saturating_sub(1);
    let mut within = scroll - offsets[h];
    let mut y = 0u16;
    while y < area.height && h < row.hunks.len() {
        let hunk = &row.hunks[h];
        let height = super::app::hunk_height(hunk);
        while within < height && y < area.height {
            let mut line = hunk_line(hunk, within, h == current);
            let row_rect = Rect::new(area.x, area.y + y, area.width, 1);
            if within == 0 {
                hits.targets.push((row_rect, Target::DiffHunk(h)));
                let control = format!("[{} accept]", control_key(app, "accept"));
                let style = if h == current {
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    dim()
                };
                if let Some(x) = right_align(&mut line, &control, area.width, style) {
                    hits.targets.push((
                        Rect::new(area.x + x, area.y + y, control.width() as u16, 1),
                        Target::HunkAccept(h),
                    ));
                }
            }
            buf.set_line(area.x, area.y + y, &line, area.width);
            within += 1;
            y += 1;
        }
        h += 1;
        within = 0;
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
        let text = if hunk.is_mode_change() {
            format!(
                "mode {} → {}",
                mode_of(&hunk.lines[0].1),
                mode_of(&hunk.lines[1].1)
            )
        } else {
            format!(
                "@@ -{} +{} @@",
                range_label(hunk.old_range.start, hunk.old_range.len()),
                range_label(hunk.new_range.start, hunk.new_range.len())
            )
        };
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

fn mode_of(line: &[u8]) -> String {
    String::from_utf8_lossy(line)
        .trim()
        .trim_start_matches("mode ")
        .to_owned()
}

/// git's `start[,len]`: 1-based start for a non-empty range, the preceding line for an
/// empty one, and `,len` omitted when it is 1 (as `git diff` prints it).
fn range_label(start: usize, len: usize) -> String {
    match len {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{len}", start + 1),
    }
}

fn line_text(bytes: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(bytes).into_owned();
    while s.ends_with('\n') || s.ends_with('\r') {
        s.pop();
    }
    s.replace('\t', "    ")
}

// ---- help --------------------------------------------------------------------------------

fn key_label(spec: &str) -> String {
    match spec {
        "up" => "↑".into(),
        "down" => "↓".into(),
        "enter" => "⏎".into(),
        "esc" => "Esc".into(),
        "tab" => "Tab".into(),
        "pageup" => "PgUp".into(),
        "pagedown" => "PgDn".into(),
        "space" => "Space".into(),
        s if s.starts_with("ctrl-") => format!("Ctrl-{}", s[5..].to_uppercase()),
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

/// The keymap's rows, then the modal's fixed keys.
fn render_help(app: &App, buf: &mut Buffer, area: Rect) {
    let rows: Vec<String> = app
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
    for (i, row) in rows.iter().take(inner.height as usize).enumerate() {
        buf.set_stringn(
            inner.x + 1,
            inner.y + i as u16,
            row,
            inner.width.saturating_sub(1) as usize,
            Style::new(),
        );
    }
    if inner.height as usize > rows.len() {
        buf.set_stringn(
            inner.x + 1,
            inner.y + inner.height - 1,
            "any key closes",
            inner.width.saturating_sub(1) as usize,
            dim(),
        );
    }
}

/// The confirm modal (§6.7), centered like the help overlay. Its numbers come from
/// `App::confirm_counts`, i.e. the held piles as they are at this frame: `Accept all <N>
/// files in <root>?` (one root) or `across <R> repos?`, then `<g> grouped upstream · <c>
/// collapsed` only when either is non-zero, then the modal's keys.
fn render_confirm(app: &App, buf: &mut Buffer, area: Rect) {
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
        .title(" accept ")
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
    use super::super::app::{Changed, testfix::*};
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

    #[test]
    fn render_ellipsize_counts_columns_not_chars() {
        assert_eq!(ellipsize("abc", 3), "abc");
        assert_eq!(ellipsize("abcd", 3), "ab…");
        assert_eq!(ellipsize("日本語.md", 5), "日本…");
        assert_eq!(ellipsize("日本語", 6), "日本語");
        assert_eq!(ellipsize("abc", 0), "");
        assert_eq!(ellipsize("abc", 1), "…");
    }

    #[test]
    fn render_range_label_matches_git() {
        assert_eq!(range_label(0, 3), "1,3");
        assert_eq!(range_label(0, 0), "0,0");
        assert_eq!(range_label(4, 0), "4,0");
        assert_eq!(range_label(9, 1), "10", "git omits `,1`");
    }

    #[test]
    fn render_plural() {
        assert_eq!(plural(0, "file"), "0 files");
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(2, "root"), "2 roots");
    }

    #[test]
    fn render_hints_and_help_follow_the_app_keymap() {
        let mut app = App::new();
        assert_eq!(
            hints(&app, 100),
            "↑↓ select  ⏎ open  n/p hunk  ^A accept all  Tab focus  r refresh  ? help  q quit"
        );
        assert_eq!(
            hints(&app, 60),
            "↑↓ select  ⏎ open  n/p hunk  ^A accept all  ? help  q quit",
            "narrow drops focus and refresh"
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
        let (frame, _) = frame_of(&app, 80, 30);
        assert!(frame.contains("x              quit"), "{frame}");
        assert!(!frame.contains("q / Ctrl-C"), "{frame}");
        assert!(
            frame.contains("A              accept the whole file"),
            "{frame}"
        );
        assert!(frame.contains("y / ⏎          confirm"), "{frame}");
        assert!(frame.contains("n / Esc        cancel"), "{frame}");
    }

    /// Under the modal the hint line names only the keys that work there: the modal's
    /// own (fixed) and the keymap's `quit` key — not the accepts and the help.
    #[test]
    fn render_hint_line_under_the_modal_names_only_its_keys() {
        let mut app = three_roots();
        app.apply(pile_event("alpha", rows_n(11, 0, 0)));
        app.select(Some(Selection::Root(root("alpha"))));
        assert_eq!(app.handle(Action::Accept), (Changed::Yes, None));
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
            hints(&app, 100).contains("a accept all in alpha  ^A accept all"),
            "{}",
            hints(&app, 100)
        );
    }

    #[test]
    fn render_hints_follow_the_selection() {
        let mut app = three_roots();
        app.select(Some(row("alpha", "f1")));
        assert_eq!(
            hints(&app, 100),
            "↑↓ select  ⏎ open  n/p hunk  a/A accept file  ^A accept all  Tab focus  r refresh  ? help  q quit"
        );
        assert_eq!(
            hints(&app, 90),
            "↑↓ select  ⏎ open  n/p hunk  a/A accept file  ^A accept all  ? help  q quit",
            "focus/refresh go when the line would not fit"
        );
        assert_eq!(
            hints(&app, 60),
            "↑↓ select  ⏎ open  n/p hunk  a/A accept file  ? help  q quit",
            "then the global accept hint"
        );
        app.handle(Action::Open);
        assert!(
            hints(&app, 100).contains("n/p hunk  a accept hunk  A accept file  ^A accept all"),
            "{}",
            hints(&app, 100)
        );
        app.select(Some(Selection::Root(root("alpha"))));
        assert!(
            hints(&app, 100).contains("n/p hunk  a accept all in alpha  ^A accept all"),
            "{}",
            hints(&app, 100)
        );
        app.select(Some(Selection::Group(
            root("beta"),
            lastcall_engine::scan::Annotation::Upstream,
        )));
        assert!(
            hints(&app, 100).contains("n/p hunk  a accept group  ^A accept all"),
            "{}",
            hints(&app, 100)
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

    #[test]
    fn render_confirm_modal_shows_live_counts() {
        let mut app = three_roots();
        app.apply(pile_event("alpha", rows_n(11, 2, 1)));
        app.select(Some(Selection::Root(root("alpha"))));
        assert_eq!(app.handle(Action::Accept), (Changed::Yes, None));
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
            .push("2 files shown · 7 more changed paths not scanned".to_owned());
        app.apply(pile_event("alpha", pile));
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(frame.contains("lastcall  3 repos · 5+ files"), "{frame}");
        assert!(frame.contains("main · 2+ files"), "{frame}");
        assert!(
            frame.contains("main · 2 files"),
            "beta stays plain: {frame}"
        );
        app.select(Some(Selection::Root(root("alpha"))));
        let (frame, _) = frame_of(&app, 100, 30);
        assert!(frame.contains("alpha  main · 2+ files"), "{frame}");
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
        let (frame, styles) = frame_of(&app, 80, 12);
        assert!(frame.contains("nothing pending across 0 roots"), "{frame}");
        assert!(
            frame.contains("lastcall  0 repos · 0 files · 0 hunks"),
            "{frame}"
        );
        assert!(frame.contains("watching nothing"), "{frame}");
        assert!(frame.contains("↑↓ select"), "{frame}");
        assert!(styles.contains("0 0..37 Reset Reset BOLD"), "{styles}");
        assert!(
            styles.contains("0 39..51 Reset Reset DIM"),
            "Accept All is dim with nothing listed: {styles}"
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
}
