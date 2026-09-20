//! Visual word wrap for the diff pane, and the one answer to "which line is on which row".
//!
//! Phase 13 deliverable A. The diff pane used to draw one line of the file on one row of
//! the screen and let `Buffer::set_line` clip whatever did not fit; a review tool that
//! hides the end of a line asks the reader to accept text they have not read. So the
//! renderer wraps.
//!
//! The scroll position is still a **line** of the diff, never a row of the screen
//! (ruling 1): `DiffCursor::scroll` keeps its meaning and nothing that stores or compares
//! it changes. Only the drawing, the hit map and the keep-visible arithmetic care about
//! rows, and all three ask [`layout`] — there is no second implementation of the mapping.
//!
//! [`wrap_words`] is not [`super::textbuf`]'s `wrap_ranges` (a hard wrap for prose in the
//! note modal, which may cut a word in half) and it is not `tour.rs`'s `wrap` (which
//! collapses whitespace). This is a diff: **every character is kept**, whitespace included,
//! and a row is measured the way ratatui draws it.

use lastcall_engine::hunks::{Hunk, Tag};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{diff_lines, hunk_block, hunk_height, hunk_offsets};

/// The gutter column every diff text row spends on its `+` / `-` / ` ` mark.
pub(super) const GUTTER: usize = 1;

/// How many rows short of the body a single wrapped line stops (ruling 2, "one wrapped line
/// never fills the pane"): the line after it is then always at least partly on screen.
const CAP_MARGIN: u16 = 3;

/// Which part of a diff line a drawn row carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Part {
    /// The line exactly as [`super::render::hunk_line`] builds it, clipped by the buffer as
    /// it always was: a hunk header (whose controls, marker, target and band are untouched
    /// by this phase), or any text line at all while wrap is off.
    Whole,
    /// The blank separator between two hunks: a row that exists, and belongs to its line
    /// for the hit map, but draws nothing at all. Not even a selection band, which is what
    /// the renderer did with it before this phase.
    /// A slice of the line's text, in **char** indices into the text without its gutter
    /// mark. The renderer repeats the mark and the colour on every row.
    Blank,
    Slice {
        start: usize,
        end: usize,
        /// The cap's marker for the last drawn row of a line that was cut (ruling 2),
        /// drawn dim and never in the line's own green or red. `None` on every other row,
        /// so a line that was not capped never loses a character to it.
        marker: Option<String>,
    },
}

/// One drawn row of the diff body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Row {
    /// The absolute diff line index (`hunk_offsets[hunk] + within`): what the hit map
    /// answers and what a selection covers.
    pub line: usize,
    /// Which hunk the row belongs to.
    pub hunk: usize,
    /// The line's offset inside the hunk's block: `0` is the header, `1..=hunk_height` its
    /// lines, and anything above that the blank separator before the next hunk.
    pub within: usize,
    pub part: Part,
}

/// `text` broken into char ranges that each fit in `width` display columns, breaking after
/// the last whitespace that fits and hard-breaking a run that is wider than the row.
///
/// Always at least one range (an empty line is one empty range), never an empty range
/// otherwise, and the ranges concatenate back to the input: this is a diff, so nothing is
/// dropped and nothing is collapsed. Whitespace that fits stays on its row; whitespace that
/// does not leads the next one, and a run of whitespace wider than the row breaks like an
/// over-long word.
///
/// A row's width is [`UnicodeWidthStr::width`] of the row's own text, not a sum of its
/// characters' widths (design review F12): an emoji written with a variation selector is
/// two cells to `Buffer::set_line` and one to a per-char sum, and drawing a row measured
/// the second way clips its last character. A break is never placed immediately before a
/// zero-width character (a combining mark, a variation selector, a joiner), which would
/// strand it on the next row away from what it modifies.
///
/// `width` of 0, and a row narrower than its own widest character, terminate: they emit one
/// character per row rather than looping, so those rows are wider than `width` and are
/// excluded from the "no part is wider than the row" property.
pub(super) fn wrap_words(text: &str, width: usize) -> Vec<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![(0, 0)];
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let end = row_end(&chars, start, width);
        out.push((start, end));
        start = end;
    }
    out
}

/// Where the row beginning at `start` ends: the exclusive char index of its last character
/// plus one. Always greater than `start`, so [`wrap_words`] terminates.
fn row_end(chars: &[char], start: usize, width: usize) -> usize {
    // How far the row can reach measuring the way ratatui draws it. `fits` grows one
    // character at a time and re-measures the whole candidate, which is what makes a
    // variation selector count for the cell it actually takes.
    let mut fit = start;
    let mut last_break = None;
    let mut i = start;
    while i < chars.len() {
        let candidate: String = chars[start..=i].iter().collect();
        if candidate.width() > width {
            break;
        }
        fit = i + 1;
        // A break may be taken *after* this character when it is whitespace and the next
        // character is not a zero-width mark that belongs to it.
        if chars[i].is_whitespace() && !starts_zero_width(chars, i + 1) {
            last_break = Some(i + 1);
        }
        i += 1;
    }
    if fit >= chars.len() {
        // The rest of the line fits on this row.
        return chars.len();
    }
    if fit == start {
        // Nothing fits: `width` is 0, or narrower than this one character. Emit the one
        // character so the caller makes progress; the row is then wider than `width`.
        return start + 1;
    }
    // Never break immediately before a zero-width character: it would land on the next row
    // without the character it modifies.
    let mut hard = fit;
    while hard > start + 1 && starts_zero_width(chars, hard) {
        hard -= 1;
    }
    match last_break {
        // A word break that leaves something on the row wins over the hard break.
        Some(b) if b > start && b <= fit => b,
        _ => hard,
    }
}

fn starts_zero_width(chars: &[char], at: usize) -> bool {
    chars.get(at).is_some_and(|c| c.width().unwrap_or(0) == 0)
}

/// The cap for a body `rows` tall: how many rows one line may take (ruling 2).
///
/// A few rows short of the body, so the line after a capped one is always at least partly
/// on screen. The promise holds whenever the body has more than [`CAP_MARGIN`] rows; below
/// that the cap is 1 and the pane is showing what it can.
pub(super) fn cap(rows: u16) -> usize {
    usize::from(rows.saturating_sub(CAP_MARGIN)).max(1)
}

/// The line's text without its gutter mark, or `None` when the line is a hunk header or a
/// separator (neither wraps).
fn body_text(hunk: &Hunk, within: usize) -> Option<String> {
    if within == 0 || within > hunk_height(hunk).saturating_sub(1) {
        return None;
    }
    Some(super::render::line_text(&hunk.lines[within - 1].1))
}

/// The gutter mark a text line repeats on every one of its rows.
pub(super) fn gutter_of(tag: Tag) -> char {
    match tag {
        Tag::Context => ' ',
        Tag::Insert => '+',
        Tag::Delete => '-',
    }
}

/// The rows one diff line occupies in a body `cols` wide and `rows` tall.
///
/// **The** answer to "which line is on which row": [`layout`], [`line_height`] and the
/// reducer's keep-visible arithmetic all reduce to this, so there is nothing to keep in
/// step. With `wrap` off, or for a header or a separator, it is one [`Part::Whole`] and the
/// body is cell-identical to the renderer before this phase.
pub(super) fn parts_of(hunk: &Hunk, within: usize, cols: u16, rows: u16, wrap: bool) -> Vec<Part> {
    if within > 0 && within > hunk_height(hunk).saturating_sub(1) {
        return vec![Part::Blank];
    }
    if !wrap {
        return vec![Part::Whole];
    }
    let Some(text) = body_text(hunk, within) else {
        return vec![Part::Whole];
    };
    let width = usize::from(cols).saturating_sub(GUTTER);
    let ranges = wrap_words(&text, width);
    if ranges.len() <= 1 {
        // One row: the whole line, drawn exactly as it was before this phase.
        return vec![Part::Whole];
    }
    let total: usize = text.chars().count();
    let cap = cap(rows);
    let mut out: Vec<Part> = Vec::with_capacity(ranges.len().min(cap));
    for (i, &(start, end)) in ranges.iter().enumerate() {
        if i + 1 == cap && ranges.len() > cap {
            let (end, marker) = cut_for_marker(&text, start, end, total, width);
            out.push(Part::Slice {
                start,
                end,
                marker: Some(marker),
            });
            break;
        }
        out.push(Part::Slice {
            start,
            end,
            marker: None,
        });
        if i + 1 == cap {
            break;
        }
    }
    out
}

/// The cap's last row: shorten it by whole characters until the marker fits beside it, and
/// return the marker that names how many characters of the line are not shown.
///
/// The count is part of the marker and shortening the row raises it, so the two are solved
/// together. A wide character is never halved, and when not even ` … +N` fits the marker
/// falls back to `…` alone.
fn cut_for_marker(
    text: &str,
    start: usize,
    end: usize,
    total: usize,
    width: usize,
) -> (usize, String) {
    let chars: Vec<char> = text.chars().collect();
    let row_width = |to: usize| chars[start..to].iter().collect::<String>().width();
    let mut end = end;
    loop {
        let marker = format!(" … +{}", total - end);
        if row_width(end) + marker.width() <= width {
            return (end, marker);
        }
        if end > start {
            end -= 1;
            continue;
        }
        // The row is empty and the counted marker still does not fit: say only that
        // something was cut.
        let short = "…".to_owned();
        return (start, short);
    }
}

/// How many rows line `at` of `hunks` takes, given the hunks' offsets.
fn line_height_with(
    hunks: &[Hunk],
    offsets: &[usize],
    at: usize,
    cols: u16,
    rows: u16,
    wrap: bool,
) -> usize {
    let Some((h, within)) = locate(hunks, offsets, at) else {
        return 1;
    };
    parts_of(&hunks[h], within, cols, rows, wrap).len()
}

/// The hunk and the offset within its block that absolute line `at` names.
fn locate(hunks: &[Hunk], offsets: &[usize], at: usize) -> Option<(usize, usize)> {
    if hunks.is_empty() {
        return None;
    }
    let h = offsets.partition_point(|&o| o <= at).saturating_sub(1);
    if h >= hunks.len() {
        return None;
    }
    let within = at - offsets[h];
    (within < hunk_block(hunks, h)).then_some((h, within))
}

/// The body's rows, from the top line `scroll` down, in drawing order.
///
/// The top row of the body is always the **first** row of `scroll`'s line (ruling 1). The
/// list is what the renderer draws, what [`super::render::HitMap::diff_rows`] records, and
/// what the reducer measures with.
pub(super) fn layout(hunks: &[Hunk], scroll: usize, cols: u16, rows: u16, wrap: bool) -> Vec<Row> {
    let mut out = Vec::new();
    if hunks.is_empty() || rows == 0 {
        return out;
    }
    let offsets = hunk_offsets(hunks);
    let total: usize = diff_lines(hunks);
    let mut at = scroll.min(total.saturating_sub(1));
    while out.len() < rows as usize && at < total {
        let Some((h, within)) = locate(hunks, &offsets, at) else {
            break;
        };
        for part in parts_of(&hunks[h], within, cols, rows, wrap) {
            if out.len() == rows as usize {
                return out;
            }
            out.push(Row {
                line: at,
                hunk: h,
                within,
                part,
            });
        }
        at += 1;
    }
    out
}

/// The last line that is **fully** on screen from top line `scroll`, or `None` when not even
/// the top line fits whole.
///
/// What the no-skip rule is stated in (design review F13, F16): a forward move may put the
/// new top at most one line past this.
pub(super) fn last_full_line(
    hunks: &[Hunk],
    scroll: usize,
    cols: u16,
    rows: u16,
    wrap: bool,
) -> Option<usize> {
    let offsets = hunk_offsets(hunks);
    let total = diff_lines(hunks);
    let mut used = 0usize;
    let mut last = None;
    let mut at = scroll;
    while at < total {
        let h = line_height_with(hunks, &offsets, at, cols, rows, wrap);
        if used + h > rows as usize {
            break;
        }
        used += h;
        last = Some(at);
        at += 1;
    }
    last
}

/// The top line a page up lands on: the lowest line from which `scroll` is still **fully**
/// on screen (a backward fill).
///
/// Page up is deliberately not page down's inverse: with rows of different heights the two
/// cannot be, and no test may claim they are. It moves at least one line unless already at
/// the top.
pub(super) fn page_up_top(
    hunks: &[Hunk],
    scroll: usize,
    cols: u16,
    rows: u16,
    wrap: bool,
) -> usize {
    if scroll == 0 {
        return 0;
    }
    let offsets = hunk_offsets(hunks);
    let mut used = line_height_with(hunks, &offsets, scroll, cols, rows, wrap);
    let mut top = scroll;
    while top > 0 {
        let h = line_height_with(hunks, &offsets, top - 1, cols, rows, wrap);
        if used + h > rows as usize {
            break;
        }
        used += h;
        top -= 1;
    }
    // At least one line, even when `scroll`'s own line is taller than the whole body.
    top.min(scroll - 1)
}

/// The top line that keeps `target` **fully** on screen, moving as little as possible from
/// the top line `from`.
///
/// What a live selection's moving end asks for ([`super::app::App::move_sel_cursor`]): the
/// reader is watching the end they are dragging, so it has to be whole, and everything they
/// already selected should stay where it is. A `target` taller than the whole body sits at
/// the top and shows what it can.
pub(super) fn keep_visible(
    hunks: &[Hunk],
    from: usize,
    target: usize,
    cols: u16,
    rows: u16,
    wrap: bool,
) -> usize {
    if target <= from {
        return target;
    }
    if last_full_line(hunks, from, cols, rows, wrap).is_some_and(|l| l >= target) {
        return from;
    }
    let offsets = hunk_offsets(hunks);
    let mut used = line_height_with(hunks, &offsets, target, cols, rows, wrap);
    let mut top = target;
    while top > from {
        let h = line_height_with(hunks, &offsets, top - 1, cols, rows, wrap);
        if used + h > rows as usize {
            break;
        }
        used += h;
        top -= 1;
    }
    top
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every part of every wrap is non-empty, the parts concatenate to the input, and no
    /// part is wider than the row — the last only where the row can hold the line's widest
    /// character, which is the documented carve-out for `width` 0.
    fn check(text: &str, width: usize) -> Vec<String> {
        let ranges = wrap_words(text, width);
        assert!(!ranges.is_empty(), "always at least one range: {text:?}");
        let chars: Vec<char> = text.chars().collect();
        let mut at = 0;
        let mut parts = Vec::new();
        for &(s, e) in &ranges {
            assert_eq!(s, at, "ranges are contiguous: {text:?} / {width}");
            assert!(
                e > s || chars.is_empty(),
                "no empty range except an empty line: {text:?}"
            );
            parts.push(chars[s..e].iter().collect::<String>());
            at = e;
        }
        assert_eq!(at, chars.len(), "the parts cover the input: {text:?}");
        assert_eq!(parts.concat(), text, "the parts concatenate to the input");
        parts
    }

    fn widest(text: &str) -> usize {
        text.chars()
            .map(|c| c.width().unwrap_or(0))
            .max()
            .unwrap_or(0)
    }

    fn check_widths(text: &str, width: usize) -> Vec<String> {
        let parts = check(text, width);
        if width >= widest(text) {
            for p in &parts {
                assert!(
                    p.width() <= width,
                    "no part is wider than the row: {p:?} in {text:?} / {width}"
                );
            }
        }
        parts
    }

    #[test]
    fn wrap_breaks_after_the_last_whitespace_that_fits() {
        let parts = check_widths("the quick brown fox", 10);
        assert_eq!(parts, vec!["the quick ", "brown fox"]);
    }

    #[test]
    fn wrap_hard_breaks_a_word_longer_than_the_row() {
        let word = "x".repeat(300);
        let parts = check_widths(&word, 40);
        assert_eq!(parts.len(), 8, "300 over 40 is 8 rows");
        assert!(parts.iter().take(7).all(|p| p.width() == 40));
    }

    #[test]
    fn wrap_never_breaks_before_a_zero_width_character() {
        // A combining acute after `e`: the break may not land between them.
        let text = "aaaa e\u{0301}bbb";
        for width in 1..=12 {
            let parts = check(text, width);
            for p in &parts {
                assert!(
                    !p.starts_with('\u{0301}'),
                    "a row may not begin with a combining mark: {parts:?} / {width}"
                );
            }
        }
    }

    #[test]
    fn wrap_keeps_whitespace_and_leads_the_next_row_with_the_overflow() {
        // Four spaces at a width of 3: the run breaks like an over-long word and every
        // space survives.
        let parts = check_widths("a    b", 3);
        assert_eq!(parts.concat(), "a    b");
        // Nothing is collapsed, unlike `tour.rs`'s `wrap`.
        assert!(parts.iter().any(|p| p.contains("  ")), "{parts:?}");
    }

    #[test]
    fn wrap_at_width_zero_and_below_one_character_terminates() {
        assert_eq!(check("abc", 0).len(), 3, "one character a row, not a loop");
        // A CJK character is two cells and the row is one.
        assert_eq!(check("世界", 1).len(), 2);
    }

    #[test]
    fn wrap_of_an_empty_line_is_one_empty_range() {
        assert_eq!(wrap_words("", 10), vec![(0, 0)]);
        assert_eq!(wrap_words("", 0), vec![(0, 0)]);
    }

    #[test]
    fn wrap_measures_a_row_the_way_ratatui_draws_it() {
        // `⚠️` is U+26A0 plus U+FE0F: two cells to `set_line`, one to a per-char sum.
        let text = "\u{26A0}\u{FE0F}\u{26A0}\u{FE0F}\u{26A0}\u{FE0F}";
        let parts = check(text, 4);
        for p in &parts {
            assert!(p.width() <= 4, "{p:?} is {} cells", p.width());
        }
    }

    // ---- the pane layout ----------------------------------------------------------------

    fn hunk_of(lines: &[(Tag, &str)]) -> Hunk {
        Hunk {
            index: 0,
            old_range: 0..1,
            new_range: 0..1,
            lines: lines
                .iter()
                .map(|(t, s)| (*t, format!("{s}\n").into_bytes()))
                .collect(),
        }
    }

    /// Lines of heights 1, 3, 3, 3, 5 in a body ten rows tall: the design review's own
    /// counter-example to "page down is page up's inverse". At 11 columns the text width
    /// is 10, so an `n`-character run of `x` takes `ceil(n / 10)` rows.
    fn uneven() -> Vec<Hunk> {
        vec![hunk_of(&[
            (Tag::Context, "short"),
            (Tag::Context, &"x".repeat(30)),
            (Tag::Insert, &"y".repeat(30)),
            (Tag::Delete, &"z".repeat(30)),
            (Tag::Context, &"w".repeat(50)),
        ])]
    }

    /// The body `uneven()` is measured in: 11 columns (10 of text) and 10 rows, so the cap
    /// is 7 and no line of it is capped.
    const UNEVEN: (u16, u16) = (11, 10);

    fn heights(hunks: &[Hunk], cols: u16, rows: u16) -> Vec<usize> {
        let offsets = hunk_offsets(hunks);
        (0..diff_lines(hunks))
            .map(|i| line_height_with(hunks, &offsets, i, cols, rows, true))
            .collect()
    }

    #[test]
    fn wrap_layout_starts_at_the_scroll_lines_first_row() {
        let hunks = uneven();
        let (cols, rows) = UNEVEN;
        assert_eq!(heights(&hunks, cols, rows), vec![1, 1, 3, 3, 3, 5]);
        for scroll in 0..diff_lines(&hunks) {
            let table = layout(&hunks, scroll, cols, rows, true);
            assert_eq!(table[0].line, scroll, "the top row is the scroll line's");
            assert!(
                matches!(&table[0].part, Part::Whole | Part::Slice { start: 0, .. }),
                "and it is that line's FIRST row: {:?}",
                table[0].part
            );
        }
    }

    #[test]
    fn wrap_layout_row_table_carries_every_drawn_row() {
        // Two hunks, so there is a separator between them.
        let hunks = vec![
            hunk_of(&[(Tag::Insert, &"a".repeat(25))]),
            hunk_of(&[(Tag::Context, "tail")]),
        ];
        let table = layout(&hunks, 0, 11, 12, true);
        // header, 3 rows of the wrapped insert, separator, header, one context line.
        assert_eq!(
            table.iter().map(|r| r.line).collect::<Vec<_>>(),
            vec![0, 1, 1, 1, 2, 3, 4]
        );
        assert!(
            matches!(table[0].part, Part::Whole),
            "the header never wraps"
        );
        assert!(
            matches!(table[4].part, Part::Blank),
            "the separator draws nothing"
        );
        assert!(matches!(table[5].part, Part::Whole), "the second header");
        assert_eq!(
            table[4].line, 2,
            "and the separator is still a line of the diff"
        );
    }

    #[test]
    fn wrap_layout_caps_a_line_and_counts_what_it_did_not_show() {
        // 200 characters at 10 columns of text is 20 rows; a body of 8 rows caps at 5.
        let hunks = vec![hunk_of(&[(Tag::Insert, &"x".repeat(200))])];
        let table = layout(&hunks, 1, 11, 8, true);
        assert_eq!(cap(8), 5);
        assert_eq!(table.len(), 5, "the cap, not the body");
        let Part::Slice { start, end, marker } = &table[4].part else {
            panic!(
                "the last row of a capped line is a slice: {:?}",
                table[4].part
            )
        };
        let marker = marker.as_deref().expect("the cap's marker");
        assert!(marker.starts_with(" … +"), "{marker:?}");
        let hidden: usize = marker.trim_start_matches(" … +").parse().expect("a count");
        assert_eq!(hidden, 200 - end, "the count is the characters not shown");
        assert!(
            (end - start) + marker.width() <= 10,
            "content and marker fit the row: {} + {}",
            end - start,
            marker.width()
        );
        for row in &table[..4] {
            assert!(
                matches!(&row.part, Part::Slice { marker: None, .. }),
                "only the last row carries the marker: {:?}",
                row.part
            );
        }
        // The promise of the cap: the line after it is at least partly on screen.
        let after = layout(&hunks, 1, 11, 8, true);
        assert!(after.len() < 8, "three rows are left for what follows");
    }

    #[test]
    fn wrap_layout_marker_falls_back_to_a_bare_ellipsis_in_a_narrow_pane() {
        // Four columns of text: ` … +N` is five and never fits.
        let hunks = vec![hunk_of(&[(Tag::Context, &"x".repeat(80))])];
        let table = layout(&hunks, 1, 5, 5, true);
        let Part::Slice { start, end, marker } = &table[1].part else {
            panic!("a slice")
        };
        assert_eq!(marker.as_deref(), Some("…"));
        assert_eq!(
            start, end,
            "the counted marker did not fit, so the row is the marker"
        );
    }

    #[test]
    fn wrap_layout_never_halves_a_wide_character_at_the_markers_edge() {
        // CJK: every character is two cells, so a row that has to give columns back to the
        // marker gives them back a whole character at a time.
        let hunks = vec![hunk_of(&[(Tag::Insert, &"世".repeat(60))])];
        let table = layout(&hunks, 1, 13, 6, true);
        let Part::Slice { start, end, marker } = &table[2].part else {
            panic!("a slice")
        };
        let marker = marker.as_deref().expect("a marker");
        let row: String = "世".repeat(end - start);
        assert_eq!(row.width(), (end - start) * 2, "no character was halved");
        assert!(row.width() + marker.width() <= 12, "{row:?} + {marker:?}");
    }

    #[test]
    fn wrap_layout_draws_something_in_a_body_of_zero_to_four_rows() {
        let hunks = uneven();
        for rows in 0..=4u16 {
            for cols in [1u16, 2, 11, 80] {
                let table = layout(&hunks, 0, cols, rows, true);
                assert!(table.len() <= rows as usize, "never past the body");
                if rows > 0 {
                    assert!(!table.is_empty(), "{rows}x{cols} draws something");
                }
                // And the reducer's questions terminate over the same geometry.
                let _ = last_full_line(&hunks, 0, cols, rows, true);
                let _ = page_up_top(&hunks, 3, cols, rows, true);
                let _ = keep_visible(&hunks, 0, 5, cols, rows, true);
            }
        }
    }

    #[test]
    fn wrap_layout_with_wrap_off_is_one_row_per_line() {
        let hunks = uneven();
        for scroll in 0..diff_lines(&hunks) {
            let table = layout(&hunks, scroll, 11, 10, false);
            assert_eq!(
                table.iter().map(|r| r.line).collect::<Vec<_>>(),
                (scroll..diff_lines(&hunks)).take(10).collect::<Vec<_>>(),
                "row = line, exactly as before this phase"
            );
            assert!(
                table
                    .iter()
                    .all(|r| matches!(r.part, Part::Whole | Part::Blank)),
                "and every row is the whole line"
            );
        }
    }

    // ---- the no-skip rules --------------------------------------------------------------

    #[test]
    fn wrap_last_full_line_is_what_forward_moves_are_clamped_to() {
        let hunks = uneven();
        let (cols, rows) = UNEVEN;
        // Heights 1, 1, 3, 3, 3, 5 in ten rows: from the top, 1+1+3+3 = 8 fits and the
        // next would be 11.
        assert_eq!(last_full_line(&hunks, 0, cols, rows, true), Some(3));
        // From line 5, the 5-row line, only it fits.
        assert_eq!(last_full_line(&hunks, 5, cols, rows, true), Some(5));
    }

    #[test]
    fn wrap_page_up_is_a_backward_fill_and_not_page_downs_inverse() {
        let hunks = uneven();
        let (cols, rows) = UNEVEN;
        // From line 5 (height 5), filling backwards: 5 + 3 + 3 = 11 > 10, so 5 + 3 = 8
        // fits and the top is line 4.
        assert_eq!(page_up_top(&hunks, 5, cols, rows, true), 4);
        assert_eq!(
            page_up_top(&hunks, 0, cols, rows, true),
            0,
            "clamped at the top"
        );
        // A fill that stops early: three lines of four rows each in a body of eight.
        let even = vec![hunk_of(&[
            (Tag::Context, &"x".repeat(40)),
            (Tag::Insert, &"y".repeat(40)),
            (Tag::Delete, &"z".repeat(40)),
        ])];
        assert_eq!(
            page_up_top(&even, 3, 11, 8, true),
            2,
            "4 + 4 fits, 4 + 4 + 4 does not"
        );
        // And it always moves, even when the scroll line alone is taller than the body.
        let tall = vec![hunk_of(&[
            (Tag::Context, "a"),
            (Tag::Context, &"x".repeat(200)),
        ])];
        assert!(page_up_top(&tall, 2, 11, 4, true) < 2);
    }

    #[test]
    fn wrap_keep_visible_moves_as_little_as_it_can() {
        let hunks = uneven();
        let (cols, rows) = UNEVEN;
        assert_eq!(
            keep_visible(&hunks, 0, 3, cols, rows, true),
            0,
            "already whole"
        );
        assert_eq!(keep_visible(&hunks, 0, 2, cols, rows, true), 0);
        // Line 5 is 5 rows; from the top it does not fit, so the pane fills backwards from
        // it: 5 + 3 = 8 fits, 5 + 3 + 3 does not.
        assert_eq!(keep_visible(&hunks, 0, 5, cols, rows, true), 4);
        assert_eq!(
            keep_visible(&hunks, 4, 1, cols, rows, true),
            1,
            "upwards is a move to it"
        );
    }

    mod proptests {
        use proptest::prelude::*;

        use super::*;

        /// 8 cases in the unit tier, `PROPTEST_CASES` (64 from the pre-push hook) when set
        /// — the house split.
        fn config() -> ProptestConfig {
            ProptestConfig {
                cases: std::env::var("PROPTEST_CASES")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(8),
                failure_persistence: None,
                ..ProptestConfig::default()
            }
        }

        /// Text built out of what breaks a naive wrap: wide characters, a combining mark,
        /// an emoji with a variation selector, a joined family, runs of spaces and a word
        /// far longer than any row.
        fn any_line() -> impl Strategy<Value = String> {
            let piece = prop_oneof![
                Just("a".to_owned()),
                Just(" ".to_owned()),
                Just("   ".to_owned()),
                Just("word".to_owned()),
                Just("字".to_owned()),
                Just("e\u{0301}".to_owned()),
                Just("\u{26A0}\u{FE0F}".to_owned()),
                Just("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}".to_owned()),
                Just("x".repeat(40)),
            ];
            prop::collection::vec(piece, 0..24).prop_map(|v| v.concat())
        }

        /// **The rule of the module.** Every character of the line is kept, in order, and
        /// no row is wider than the pane can draw.
        #[test]
        fn wrap_words_keeps_every_character_and_fits_every_row() {
            proptest!(config(), |(text in any_line(), width in 1usize..40)| {
                let chars: Vec<char> = text.chars().collect();
                let ranges = wrap_words(&text, width);
                prop_assert!(!ranges.is_empty());
                let mut at = 0;
                let mut rebuilt = String::new();
                for &(s, e) in &ranges {
                    prop_assert_eq!(s, at);
                    prop_assert!(e > s || chars.is_empty());
                    let row: String = chars[s..e].iter().collect();
                    // The carve-out: a row narrower than its own single character.
                    if e - s > 1 {
                        prop_assert!(
                            row.width() <= width,
                            "row {:?} is {} cells in {}",
                            row, row.width(), width
                        );
                    }
                    rebuilt.push_str(&row);
                    at = e;
                }
                prop_assert_eq!(at, chars.len());
                prop_assert_eq!(rebuilt, text);
            });
        }

        /// F26: with wrap off the row table is `scroll..scroll + rows`, so the body is the
        /// pre-phase one whatever the hunks and the size.
        #[test]
        fn wrap_off_row_table_is_the_pre_phase_one() {
            fn any_hunk() -> impl Strategy<Value = Vec<(Tag, String)>> {
                let tags = prop_oneof![Just(Tag::Context), Just(Tag::Insert), Just(Tag::Delete)];
                prop::collection::vec((tags, any_line()), 1..8)
            }
            proptest!(config(), |(
                a in any_hunk(),
                b in any_hunk(),
                scroll in 0usize..12,
                cols in 1u16..90,
                rows in 0u16..20,
            )| {
                let mk = |v: Vec<(Tag, String)>| Hunk {
                    index: 0,
                    old_range: 0..1,
                    new_range: 0..1,
                    lines: v.into_iter().map(|(t, s)| (t, s.into_bytes())).collect(),
                };
                let hunks = vec![mk(a), mk(b)];
                let total = diff_lines(&hunks);
                let scroll = scroll.min(total - 1);
                let table = layout(&hunks, scroll, cols, rows, false);
                let want: Vec<usize> = (scroll..total).take(rows as usize).collect();
                prop_assert_eq!(table.iter().map(|r| r.line).collect::<Vec<_>>(), want);
                prop_assert!(table.iter().all(|r| matches!(r.part, Part::Whole | Part::Blank)));
            });
        }
    }
}
