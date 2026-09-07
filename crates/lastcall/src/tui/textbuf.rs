//! The editable text buffer behind the note modal and the inline editor (Phase 8
//! deliverable 4).
//!
//! One buffer, two consumers, one rule that governs the whole file:
//!
//! > **`TextBuf::from(t).text() == t` for every UTF-8 `t`.**
//!
//! A review tool that edits a file has to hand back exactly what it was given plus the
//! user's own change — nothing else. So the line **endings live beside the text, never
//! inside it** ([`Line::end`]), a file with no trailing newline keeps not having one
//! ([`TextBuf::last_terminated`]), a lone `\r` in the middle of a line is an ordinary
//! character, and a BOM is simply char 0 of line 0 (zero columns wide). The proptest at the
//! bottom is the statement of that rule; every operation below is written so that it holds
//! afterwards too (design review F6).
//!
//! **Columns.** `cursor.col` is a **char index** into the line — insertion points are
//! char-exact, so no operation can land inside a code point. What the *screen* measures is a
//! different number: [`col_width`] walks the chars with `unicode-width`, and a `\t` advances
//! to the next multiple of [`TAB_STOP`] while the buffer keeps the tab itself. `want_col` —
//! the sticky column that survives a vertical move over a short line — is a display column,
//! because that is what the eye tracks.
//!
//! **`top` means what the caller's wrap mode says it means.** In [`Wrap::None`] (the inline
//! editor) it is the first **logical line** on screen and [`TextBuf::left`] scrolls sideways;
//! in [`Wrap::Soft`] (the note modal) long lines break for display only, so it is the first
//! **display row** and `left` stays 0. A buffer is rendered in one mode for its whole life —
//! the modal wraps, the editor does not — so the two readings never meet on one value.
//!
//! **A terminated buffer's final newline is not editable** (verifier (a) F9). `from("\n")`
//! is one line with `last_terminated = true`; the cursor cannot pass the end of the last
//! line, so `Delete` there does nothing and a user cannot strip a file's trailing newline
//! from inside lastcall. That is vim's `eol` semantics and it is consistent both ways —
//! `""` plus a `Newline` is two unterminated lines that also round-trip to `"\n"`. It is
//! irrelevant to the note modal; in the inline editor it is a documented limit, and
//! `$EDITOR` (`shift-i`) is the way out of it.
//!
//! Pure: no I/O, no `Instant`, no terminal. The reducer owns a `TextBuf`; `render` reads a
//! [`View`] out of it.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::input::EditKey;

/// A tab advances to the next multiple of this. Eight is what `cat`, `less`, git's own diff
/// output and every terminal default agree on, so a file looks in the editor the way it
/// looks in the diff pane beside it.
pub const TAB_STOP: usize = 8;

/// How one line ends. Kept beside the text so `\r\n` survives an edit that never touches the
/// end of the line, and so a CRLF file does not silently become an LF one on save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Lf,
    CrLf,
}

impl Ending {
    pub fn as_str(self) -> &'static str {
        match self {
            Ending::Lf => "\n",
            Ending::CrLf => "\r\n",
        }
    }
}

/// One line: its characters, without the terminator, and the terminator it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub end: Ending,
}

/// The insertion point: a line index and a **char** index inside that line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

/// Whether the renderer breaks long lines (the note modal) or scrolls past them (the inline
/// editor). See the module note on what `top` means under each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wrap {
    Soft,
    None,
}

/// What one frame shows: the rows as strings of display cells, where the caret sits in them,
/// and which of them run off the right edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// The visible rows, tabs already expanded to spaces and the window applied. Shorter
    /// than `rows` only when the buffer ends first.
    pub rows: Vec<String>,
    /// `(row, column)` of the caret **within `rows`**, both zero-based; `column` is a
    /// display column, so it indexes the string's cells and not its chars.
    pub caret: (usize, usize),
    /// The logical line `rows[0]` starts in, for a line-number gutter.
    pub first_line: usize,
    /// Per row: content continues past the right edge (the `→` marker, deliverable 8).
    /// Always all-`false` under [`Wrap::Soft`], which has no right edge to fall off.
    pub clipped: Vec<bool>,
}

/// The display column just after the first `chars` characters of `text`.
///
/// The one place tabs and wide characters are turned into columns; every caller — the caret,
/// the sticky column, the renderer — goes through it, so they cannot disagree.
pub fn col_width(text: &str, chars: usize) -> usize {
    let mut col = 0;
    for ch in text.chars().take(chars) {
        col += cell_width(ch, col);
    }
    col
}

/// One character's width **at** column `col` (a tab's width depends on where it starts).
fn cell_width(ch: char, col: usize) -> usize {
    if ch == '\t' {
        TAB_STOP - col % TAB_STOP
    } else {
        ch.width().unwrap_or(0)
    }
}

/// The char index whose display column is nearest to (and not past) `want`.
fn char_at_col(text: &str, want: usize) -> usize {
    let mut col = 0;
    for (i, ch) in text.chars().enumerate() {
        let next = col + cell_width(ch, col);
        if next > want {
            return i;
        }
        col = next;
    }
    text.chars().count()
}

/// What kind of run a char belongs to, for word motion: a word is a run of alphanumerics,
/// and a run of punctuation is a word of its own (so `foo.bar` is three moves, not one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(ch: char) -> Class {
    if ch.is_whitespace() {
        Class::Space
    } else if ch.is_alphanumeric() || ch == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextBuf {
    pub lines: Vec<Line>,
    /// Whether the last line's ending is part of the text. `false` is a file that does not
    /// end in a newline — a thing an editor must be able to leave alone.
    pub last_terminated: bool,
    pub cursor: Pos,
    /// First visible row: a logical line under [`Wrap::None`], a display row under
    /// [`Wrap::Soft`].
    pub top: usize,
    /// First visible display column. Always 0 under [`Wrap::Soft`].
    pub left: usize,
    /// The display column a vertical move aims for, kept across a short line. Cleared by
    /// every horizontal move and every edit.
    pub want_col: Option<usize>,
    /// Bumped by every edit; `saved` records the value at the last save, so `dirty` is a
    /// comparison and not a second copy of the text.
    generation: u64,
    saved: u64,
}

impl From<&str> for TextBuf {
    /// Parse `text` into lines, endings and the trailing-newline flag, cursor at the top.
    ///
    /// A `\r` counts as part of a CRLF **only** when a `\n` follows it: a lone `\r` — an old
    /// Mac line break, or a stray control byte — is a character of the line and comes back
    /// out as one. That distinction is the whole reason the round trip holds.
    fn from(text: &str) -> Self {
        let mut lines: Vec<Line> = Vec::new();
        let mut rest = text;
        while let Some(i) = rest.find('\n') {
            let seg = &rest[..i];
            let (body, end) = match seg.strip_suffix('\r') {
                Some(body) => (body, Ending::CrLf),
                None => (seg, Ending::Lf),
            };
            lines.push(Line {
                text: body.to_owned(),
                end,
            });
            rest = &rest[i + 1..];
        }
        let last_terminated = rest.is_empty() && !lines.is_empty();
        if !last_terminated {
            let end = dominant(&lines);
            lines.push(Line {
                text: rest.to_owned(),
                end,
            });
        }
        Self {
            lines,
            last_terminated,
            cursor: Pos::default(),
            top: 0,
            left: 0,
            want_col: None,
            generation: 0,
            saved: 0,
        }
    }
}

/// The ending a line the file did not spell out should get: whichever the file uses more,
/// `Lf` for a file with nothing to go on.
fn dominant(lines: &[Line]) -> Ending {
    let crlf = lines.iter().filter(|l| l.end == Ending::CrLf).count();
    if crlf * 2 > lines.len() {
        Ending::CrLf
    } else {
        Ending::Lf
    }
}

impl Default for TextBuf {
    fn default() -> Self {
        Self::from("")
    }
}

impl TextBuf {
    /// [`TextBuf::from`] with the cursor put at the start of one-based line `line`, clamped
    /// to the buffer — how the inline editor opens at a hunk (deliverable 8).
    pub fn open(text: &str, line: usize) -> Self {
        let mut buf = Self::from(text);
        buf.cursor.line = line.saturating_sub(1).min(buf.lines.len() - 1);
        buf
    }

    /// Exactly the bytes this buffer stands for. The inverse of [`TextBuf::from`].
    pub fn text(&self) -> String {
        let mut out = String::new();
        let last = self.lines.len() - 1;
        for (i, line) in self.lines.iter().enumerate() {
            out.push_str(&line.text);
            if i < last || self.last_terminated {
                out.push_str(line.end.as_str());
            }
        }
        out
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Whether anything has been typed since the buffer was opened or last saved.
    pub fn dirty(&self) -> bool {
        self.generation != self.saved
    }

    /// Called after a successful save: what is on disk is what is in the buffer.
    pub fn mark_saved(&mut self) {
        self.saved = self.generation;
    }

    /// The caret's display column on its own line.
    pub fn caret_col(&self) -> usize {
        col_width(&self.lines[self.cursor.line].text, self.cursor.col)
    }

    fn cur(&self) -> &str {
        &self.lines[self.cursor.line].text
    }

    fn cur_chars(&self) -> usize {
        self.cur().chars().count()
    }

    /// The byte offset of char index `col` in `text` — the one conversion every edit needs.
    fn byte_of(text: &str, col: usize) -> usize {
        text.char_indices()
            .nth(col)
            .map(|(i, _)| i)
            .unwrap_or(text.len())
    }

    fn touched(&mut self) {
        self.generation += 1;
        self.want_col = None;
    }

    // ---- editing -------------------------------------------------------------------------

    pub fn insert_char(&mut self, ch: char) {
        if ch == '\n' {
            self.newline();
            return;
        }
        let at = Self::byte_of(self.cur(), self.cursor.col);
        self.lines[self.cursor.line].text.insert(at, ch);
        self.cursor.col += 1;
        self.touched();
    }

    /// Insert `text` at the cursor, splitting on `\n` — how a bracketed paste lands, as one
    /// edit rather than a key storm.
    ///
    /// A `\r\n` in the pasted text takes the **buffer's dominant ending**, not `CrLf`
    /// (verifier (a) F3). A clipboard is not a file: a traceback copied out of PowerShell
    /// pastes CRLF, and giving those lines `CrLf` inside an LF buffer mixes endings into a
    /// file the user never touched that way — and, in the note modal, writes a ledger note
    /// whose export reads `line^M` on every pasted line. `CrLf` survives a paste only into
    /// a buffer that already uses it. The round trip is unaffected: that is
    /// [`TextBuf::from`], which does read a file and does keep every ending it finds.
    pub fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let pasted_end = dominant(&self.lines);
        let at = Self::byte_of(self.cur(), self.cursor.col);
        let tail = self.lines[self.cursor.line].text.split_off(at);
        let end = self.lines[self.cursor.line].end;
        let mut rest = text;
        while let Some(i) = rest.find('\n') {
            let seg = &rest[..i];
            let (body, seg_end) = match seg.strip_suffix('\r') {
                Some(body) => (body, pasted_end),
                None => (seg, end),
            };
            self.lines[self.cursor.line].text.push_str(body);
            self.lines[self.cursor.line].end = seg_end;
            self.cursor.line += 1;
            self.lines.insert(
                self.cursor.line,
                Line {
                    text: String::new(),
                    end,
                },
            );
            rest = &rest[i + 1..];
        }
        self.lines[self.cursor.line].text.push_str(rest);
        self.cursor.col = self.cur_chars();
        self.lines[self.cursor.line].text.push_str(&tail);
        self.touched();
    }

    /// Split the line at the cursor. The new line inherits the ending of the line it came
    /// from, so one CRLF line cannot quietly seed an LF one.
    pub fn newline(&mut self) {
        let at = Self::byte_of(self.cur(), self.cursor.col);
        let tail = self.lines[self.cursor.line].text.split_off(at);
        let end = self.lines[self.cursor.line].end;
        self.cursor.line += 1;
        self.lines
            .insert(self.cursor.line, Line { text: tail, end });
        self.cursor.col = 0;
        // A buffer whose last line was unterminated has grown a line above it; the flag
        // still describes the *new* last line, which is that same unterminated one.
        self.touched();
    }

    /// Delete the char before the cursor, joining with the line above at column 0.
    pub fn backspace(&mut self) -> bool {
        if self.cursor.col > 0 {
            let at = Self::byte_of(self.cur(), self.cursor.col - 1);
            self.lines[self.cursor.line].text.remove(at);
            self.cursor.col -= 1;
            self.touched();
            return true;
        }
        if self.cursor.line == 0 {
            return false;
        }
        self.join_up();
        true
    }

    /// Delete the char under the cursor, pulling the next line up at end of line.
    pub fn delete(&mut self) -> bool {
        if self.cursor.col < self.cur_chars() {
            let at = Self::byte_of(self.cur(), self.cursor.col);
            self.lines[self.cursor.line].text.remove(at);
            self.touched();
            return true;
        }
        if self.cursor.line + 1 >= self.lines.len() {
            return false;
        }
        self.cursor.line += 1;
        self.cursor.col = 0;
        self.join_up();
        true
    }

    /// Merge the cursor's line into the one above it, cursor at the seam. The **upper**
    /// line's ending is the one that goes — it is the terminator that just stopped existing.
    ///
    /// So the surviving line takes the **lower** line's ending, and in a mixed-ending file
    /// a join can change a byte outside the join itself (`"a\nb\r\n"`, Backspace at the
    /// start of line 2, gives `"ab\r\n"` — verifier (a) F8). That is the defensible
    /// reading: the terminator that survives is the one that was never deleted. It is
    /// recorded here so the inline editor's tests do not later read it as a bug.
    fn join_up(&mut self) {
        let line = self.lines.remove(self.cursor.line);
        self.cursor.line -= 1;
        self.cursor.col = self.cur_chars();
        self.lines[self.cursor.line].text.push_str(&line.text);
        self.lines[self.cursor.line].end = line.end;
        // Joining away the last line leaves the new last line unterminated only if the one
        // that vanished was: `last_terminated` describes the file's tail, which just moved.
        self.touched();
    }

    /// `Ctrl-K`: from the cursor to the end of the line; on an already-empty tail, eat the
    /// line break instead, which is what every readline-shaped editor does.
    pub fn kill_to_end(&mut self) -> bool {
        if self.cursor.col < self.cur_chars() {
            let at = Self::byte_of(self.cur(), self.cursor.col);
            self.lines[self.cursor.line].text.truncate(at);
            self.touched();
            return true;
        }
        self.delete()
    }

    /// `Alt-Backspace` / `Ctrl-W`: delete back to where [`TextBuf::word_left`] would land.
    pub fn word_backspace(&mut self) -> bool {
        let from = self.cursor;
        let to = self.word_left_pos();
        if to == from {
            return false;
        }
        if to.line == from.line {
            let a = Self::byte_of(self.cur(), to.col);
            let b = Self::byte_of(self.cur(), from.col);
            self.lines[from.line].text.replace_range(a..b, "");
            self.cursor = to;
            self.touched();
            return true;
        }
        // Only ever one line up: `word_left` steps to the end of the previous line and
        // stops there, so the join below is the same one `backspace` does at column 0.
        self.join_up();
        true
    }

    // ---- motion --------------------------------------------------------------------------

    pub fn left(&mut self) {
        self.want_col = None;
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        } else if self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.col = self.cur_chars();
        }
    }

    pub fn right(&mut self) {
        self.want_col = None;
        if self.cursor.col < self.cur_chars() {
            self.cursor.col += 1;
        } else if self.cursor.line + 1 < self.lines.len() {
            self.cursor.line += 1;
            self.cursor.col = 0;
        }
    }

    pub fn up(&mut self) {
        self.vertical(-1);
    }

    pub fn down(&mut self) {
        self.vertical(1);
    }

    pub fn page_up(&mut self, rows: usize) {
        self.vertical(-(rows.max(1) as isize));
    }

    pub fn page_down(&mut self, rows: usize) {
        self.vertical(rows.max(1) as isize);
    }

    /// Move `delta` lines, keeping the display column the user is aiming for: a short line
    /// in between puts the caret at its end, and the next move returns to the old column.
    fn vertical(&mut self, delta: isize) {
        let want = self.want_col.unwrap_or_else(|| self.caret_col());
        let target = (self.cursor.line as isize + delta).clamp(0, self.lines.len() as isize - 1);
        self.cursor.line = target as usize;
        self.cursor.col = char_at_col(self.cur(), want);
        self.want_col = Some(want);
    }

    pub fn home(&mut self) {
        self.want_col = None;
        self.cursor.col = 0;
    }

    pub fn end(&mut self) {
        self.want_col = None;
        self.cursor.col = self.cur_chars();
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_left_pos();
        self.want_col = None;
    }

    pub fn word_right(&mut self) {
        self.cursor = self.word_right_pos();
        self.want_col = None;
    }

    fn word_left_pos(&self) -> Pos {
        let mut pos = self.cursor;
        if pos.col == 0 {
            if pos.line == 0 {
                return pos;
            }
            pos.line -= 1;
            pos.col = self.lines[pos.line].text.chars().count();
            return pos;
        }
        let chars: Vec<char> = self.lines[pos.line].text.chars().collect();
        while pos.col > 0 && class(chars[pos.col - 1]) == Class::Space {
            pos.col -= 1;
        }
        if pos.col == 0 {
            return pos;
        }
        let run = class(chars[pos.col - 1]);
        while pos.col > 0 && class(chars[pos.col - 1]) == run {
            pos.col -= 1;
        }
        pos
    }

    fn word_right_pos(&self) -> Pos {
        let mut pos = self.cursor;
        let chars: Vec<char> = self.lines[pos.line].text.chars().collect();
        if pos.col >= chars.len() {
            if pos.line + 1 < self.lines.len() {
                pos.line += 1;
                pos.col = 0;
            }
            return pos;
        }
        while pos.col < chars.len() && class(chars[pos.col]) == Class::Space {
            pos.col += 1;
        }
        if pos.col >= chars.len() {
            return pos;
        }
        let run = class(chars[pos.col]);
        while pos.col < chars.len() && class(chars[pos.col]) == run {
            pos.col += 1;
        }
        pos
    }

    /// Put the cursor at the display column `col` of logical line `line`, both clamped —
    /// what a mouse click in the editor pane means (deliverable 8).
    pub fn click(&mut self, line: usize, col: usize) {
        self.cursor.line = line.min(self.lines.len() - 1);
        self.cursor.col = char_at_col(self.cur(), col);
        self.want_col = None;
    }

    // ---- the window ----------------------------------------------------------------------

    /// Scroll so the caret is on screen and render the `rows`×`width` window.
    ///
    /// Takes `&mut self` because scrolling **is** buffer state: the same call on the next
    /// frame must not jump, and a `Resize` re-clamps by being asked for the new size (F19).
    pub fn viewport(&mut self, rows: usize, width: usize, wrap: Wrap) -> View {
        if rows == 0 || width == 0 {
            return View {
                rows: Vec::new(),
                caret: (0, 0),
                first_line: self.cursor.line,
                clipped: Vec::new(),
            };
        }
        match wrap {
            Wrap::None => self.viewport_flat(rows, width),
            Wrap::Soft => self.viewport_wrapped(rows, width),
        }
    }

    fn viewport_flat(&mut self, rows: usize, width: usize) -> View {
        if self.cursor.line < self.top {
            self.top = self.cursor.line;
        } else if self.cursor.line >= self.top + rows {
            self.top = self.cursor.line + 1 - rows;
        }
        self.top = self.top.min(self.lines.len().saturating_sub(1));
        let caret_col = self.caret_col();
        if caret_col < self.left {
            self.left = caret_col;
        } else if caret_col >= self.left + width {
            self.left = caret_col + 1 - width;
        }
        self.view(rows, width)
    }

    /// The unwrapped window **at the scroll the buffer already has** — no clamping, no
    /// mutation (Phase 8 deliverable 8).
    ///
    /// [`TextBuf::viewport`] is the *reducer's* call: it scrolls to the caret, which is
    /// buffer state and must survive to the next frame. This is the *renderer's*, which is
    /// handed a `&App` and must not move anything it is only drawing — and which would
    /// otherwise have to clone a whole file's worth of lines every frame to be allowed to.
    /// The inline editor keeps the two honest by re-clamping through `viewport` after every
    /// key and every resize, so what this returns is always a window the caret is inside;
    /// a caret outside it anyway is reported at the nearest edge rather than underflowing.
    pub fn view(&self, rows: usize, width: usize) -> View {
        if rows == 0 || width == 0 {
            return View {
                rows: Vec::new(),
                caret: (0, 0),
                first_line: self.top,
                clipped: Vec::new(),
            };
        }
        let caret_col = self.caret_col();
        let mut out = Vec::new();
        let mut clipped = Vec::new();
        for line in self.lines.iter().skip(self.top).take(rows) {
            let (text, cut) = render_line(&line.text, self.left, width);
            out.push(text);
            clipped.push(cut);
        }
        View {
            caret: (
                self.cursor.line.saturating_sub(self.top).min(rows - 1),
                caret_col.saturating_sub(self.left).min(width - 1),
            ),
            rows: out,
            first_line: self.top,
            clipped,
        }
    }

    fn viewport_wrapped(&mut self, rows: usize, width: usize) -> View {
        self.left = 0;
        // Every display row of the whole buffer: the note modal is prose a reader typed, so
        // wrapping it whole costs nothing and keeps the scroll arithmetic one subtraction.
        let mut all: Vec<(usize, usize, usize)> = Vec::new();
        let mut caret_row = 0;
        for (li, line) in self.lines.iter().enumerate() {
            for (a, b) in wrap_ranges(&line.text, width) {
                if li == self.cursor.line && self.cursor.col >= a && self.cursor.col <= b {
                    caret_row = all.len();
                }
                all.push((li, a, b));
            }
        }
        if caret_row < self.top {
            self.top = caret_row;
        } else if caret_row >= self.top + rows {
            self.top = caret_row + 1 - rows;
        }
        self.top = self.top.min(all.len().saturating_sub(1));
        let mut out = Vec::new();
        for (li, a, b) in all.iter().skip(self.top).take(rows) {
            let text: String = self.lines[*li]
                .text
                .chars()
                .skip(*a)
                .take(b - a)
                .collect::<String>();
            out.push(expand_tabs(&text, col_of(&self.lines[*li].text, *a)));
        }
        let (line, a, _) = all[caret_row];
        let caret_col = col_width(&self.lines[line].text, self.cursor.col)
            - col_width(&self.lines[line].text, a);
        View {
            caret: (caret_row - self.top, caret_col),
            rows: out,
            first_line: all.get(self.top).map(|(li, _, _)| *li).unwrap_or(0),
            clipped: vec![false; all.len().saturating_sub(self.top).min(rows)],
        }
    }
}

/// The display column char `at` starts at.
fn col_of(text: &str, at: usize) -> usize {
    col_width(text, at)
}

/// `text` rendered as display cells with tabs expanded, starting at display column `start`.
fn expand_tabs(text: &str, start: usize) -> String {
    let mut out = String::new();
    let mut col = start;
    for ch in text.chars() {
        let w = cell_width(ch, col);
        if ch == '\t' {
            out.extend(std::iter::repeat_n(' ', w));
        } else {
            out.push(ch);
        }
        col += w;
    }
    out
}

/// One line as the `width` cells starting at display column `from`, and whether anything is
/// left over on the right.
///
/// A tab or a wide character straddling either edge becomes the spaces it covers on screen:
/// a half-drawn `字` is a lie about where the columns are, and the caret arithmetic above
/// counts cells.
fn render_line(text: &str, from: usize, width: usize) -> (String, bool) {
    let mut out = String::new();
    let mut col = 0;
    let mut clipped = false;
    for ch in text.chars() {
        let w = cell_width(ch, col);
        let next = col + w;
        if next <= from {
            col = next;
            continue;
        }
        if col < from {
            // Straddles the left edge: only the part inside the window is drawn.
            let visible = next - from;
            out.extend(std::iter::repeat_n(' ', visible.min(width)));
            clipped |= visible > width;
            col = next;
            continue;
        }
        let at = col - from;
        if at >= width {
            clipped = true;
            break;
        }
        if ch == '\t' || (w > 1 && at + w > width) {
            let room = width - at;
            out.extend(std::iter::repeat_n(' ', w.min(room)));
            clipped |= w > room;
        } else {
            out.push(ch);
        }
        col = next;
    }
    (out, clipped)
}

/// `text` broken into char ranges of at most `width` display columns. Hard wrapping, not word
/// wrapping: a note is prose, and a word cut in half is still readable while a rule that
/// hides the caret is not. Always at least one range, so an empty line still has a row.
fn wrap_ranges(text: &str, width: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut col = 0;
    for (i, ch) in text.chars().enumerate() {
        let w = cell_width(ch, col);
        if col + w > width && i > start {
            out.push((start, i));
            start = i;
            col = 0;
        }
        col += cell_width(ch, col);
    }
    out.push((start, text.chars().count()));
    out
}

impl TextBuf {
    /// Fold one [`EditKey`] in; `page` is the visible row count a page key moves by.
    ///
    /// Returns whether anything moved or changed — a `Backspace` at the very start of the
    /// buffer is not a frame worth drawing.
    pub fn apply(&mut self, key: EditKey, page: usize) -> bool {
        let before = (self.cursor, self.generation);
        match key {
            EditKey::Insert(text) => self.insert_str(&text),
            EditKey::Newline => self.newline(),
            EditKey::Backspace => {
                return self.backspace();
            }
            EditKey::Delete => {
                return self.delete();
            }
            EditKey::WordBackspace => {
                return self.word_backspace();
            }
            EditKey::KillToEnd => {
                return self.kill_to_end();
            }
            EditKey::Left => self.left(),
            EditKey::Right => self.right(),
            EditKey::Up => self.up(),
            EditKey::Down => self.down(),
            EditKey::WordLeft => self.word_left(),
            EditKey::WordRight => self.word_right(),
            EditKey::Home => self.home(),
            EditKey::End => self.end(),
            EditKey::PageUp => self.page_up(page),
            EditKey::PageDown => self.page_down(page),
        }
        before != (self.cursor, self.generation)
    }

    /// A one-line summary the editor header uses: `line <n>/<total>`.
    pub fn position_label(&self) -> String {
        format!("line {}/{}", self.cursor.line + 1, self.lines.len())
    }
}

/// The width of a rendered row, for the callers that lay out around it.
pub fn row_width(row: &str) -> usize {
    row.width()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three lines with a tab, a wide character and a combining mark, so every column
    /// assertion below is about something the naive `len()` would get wrong.
    fn fixture() -> TextBuf {
        TextBuf::from("alpha beta\n\tw=字 e\u{0301}q\nlast")
    }

    #[test]
    fn textbuf_parses_endings_beside_the_text_and_writes_them_back() {
        let buf = TextBuf::from("a\r\nb\nc");
        assert_eq!(
            buf.lines,
            vec![
                Line {
                    text: "a".into(),
                    end: Ending::CrLf
                },
                Line {
                    text: "b".into(),
                    end: Ending::Lf
                },
                Line {
                    text: "c".into(),
                    end: Ending::Lf
                },
            ]
        );
        assert!(!buf.last_terminated);
        assert_eq!(buf.text(), "a\r\nb\nc");

        // A lone `\r` is a character, not a terminator: the one case a naive split ruins.
        let lone = TextBuf::from("\r");
        assert_eq!(lone.lines.len(), 1);
        assert_eq!(lone.lines[0].text, "\r");
        assert_eq!(lone.text(), "\r");

        // An empty buffer has one empty line and no terminator.
        let empty = TextBuf::from("");
        assert_eq!(empty.lines.len(), 1);
        assert!(!empty.last_terminated);
        assert_eq!(empty.text(), "");

        // A bare newline is one empty *terminated* line, and `a\n` is one line, not two.
        assert_eq!(TextBuf::from("\n").text(), "\n");
        assert_eq!(TextBuf::from("a\n").lines.len(), 1);
        assert!(TextBuf::from("a\n").last_terminated);

        // A BOM is char 0 of line 0 and no columns wide.
        let bom = TextBuf::from("\u{FEFF}hi\n");
        assert_eq!(bom.lines[0].text, "\u{FEFF}hi");
        assert_eq!(col_width(&bom.lines[0].text, 1), 0);
        assert_eq!(bom.text(), "\u{FEFF}hi\n");
    }

    #[test]
    fn textbuf_insert_char_and_str_and_newline() {
        let mut buf = TextBuf::from("ab\n");
        buf.cursor = Pos { line: 0, col: 1 };
        buf.insert_char('X');
        assert_eq!(buf.text(), "aXb\n");
        assert_eq!(buf.cursor.col, 2);
        assert!(buf.dirty());
        buf.mark_saved();
        assert!(!buf.dirty());

        // A paste lands as one edit and splits on newlines, CRLF included — and the pasted
        // lines take this buffer's ending, which is LF (see the dedicated test below).
        buf.insert_str("1\r\n2\n");
        assert_eq!(buf.text(), "aX1\n2\nb\n");
        assert_eq!(buf.cursor, Pos { line: 2, col: 0 });
        assert!(buf.dirty(), "a paste is a change");

        // An explicit newline splits and the new line inherits the ending it came from.
        let mut crlf = TextBuf::from("one\r\n");
        crlf.cursor.col = 1;
        crlf.newline();
        assert_eq!(crlf.text(), "o\r\nne\r\n");
        assert_eq!(crlf.lines[0].end, Ending::CrLf);

        // An unterminated file stays unterminated however much is typed into it.
        let mut tail = TextBuf::from("x");
        tail.end();
        tail.insert_str("y\nz");
        assert_eq!(tail.text(), "xy\nz");
        assert!(!tail.last_terminated);
    }

    /// Verifier (a) F3. A clipboard is not a file: the CRLF in a paste says where the text
    /// was copied from, not what this buffer's lines look like. So a pasted `\r\n` takes the
    /// buffer's dominant ending, and `CrLf` survives only into a buffer that already uses it.
    #[test]
    fn textbuf_paste_of_crlf_takes_the_buffers_dominant_ending() {
        // An LF file: the paste is flattened to LF and no `\r` reaches the text.
        let mut lf = TextBuf::from("a\nb\n");
        lf.cursor = Pos { line: 0, col: 1 };
        lf.insert_str("1\r\n2");
        assert_eq!(lf.text(), "a1\n2\nb\n");
        assert!(
            lf.lines.iter().all(|l| l.end == Ending::Lf),
            "no CRLF line was seeded: {:?}",
            lf.lines
        );

        // The note modal's buffer is empty, so it is LF by default: the ledger note (and
        // the export line built from it) is free of `^M`.
        let mut note = TextBuf::default();
        note.insert_str("Traceback\r\n  line 1\r\n");
        assert_eq!(note.text(), "Traceback\n  line 1\n");

        // A CRLF file keeps CRLF: pasting into it must not seed an LF line either.
        let mut crlf = TextBuf::from("a\r\nb\r\n");
        crlf.cursor = Pos { line: 0, col: 1 };
        crlf.insert_str("1\r\n2");
        assert_eq!(crlf.text(), "a1\r\n2\r\nb\r\n");
        assert!(
            crlf.lines.iter().all(|l| l.end == Ending::CrLf),
            "{:?}",
            crlf.lines
        );

        // Reading a mixed file back is a different job and still keeps every ending it
        // finds — the round-trip property is `From`, not `insert_str`.
        assert_eq!(TextBuf::from("a\r\nb\n").text(), "a\r\nb\n");
    }

    #[test]
    fn textbuf_backspace_delete_and_join() {
        let mut buf = TextBuf::from("ab\ncd\n");
        buf.cursor = Pos { line: 1, col: 0 };
        assert!(buf.backspace(), "column 0 joins with the line above");
        assert_eq!(buf.text(), "abcd\n");
        assert_eq!(buf.cursor, Pos { line: 0, col: 2 });
        assert!(buf.backspace());
        assert_eq!(buf.text(), "acd\n");

        buf.cursor = Pos { line: 0, col: 0 };
        assert!(!buf.backspace(), "nothing before the start of the buffer");
        assert!(buf.delete());
        assert_eq!(buf.text(), "cd\n");

        let mut two = TextBuf::from("a\nb");
        two.cursor = Pos { line: 0, col: 1 };
        assert!(
            two.delete(),
            "at end of line, delete pulls the next line up"
        );
        assert_eq!(two.text(), "ab");
        assert_eq!(
            two.cursor,
            Pos { line: 0, col: 1 },
            "the cursor sits at the seam"
        );
        two.end();
        assert!(!two.delete(), "nothing past the end of the buffer");
    }

    #[test]
    fn textbuf_kill_to_end_and_word_backspace() {
        let mut buf = TextBuf::from("keep this\nnext\n");
        buf.cursor = Pos { line: 0, col: 4 };
        assert!(buf.kill_to_end());
        assert_eq!(buf.text(), "keep\nnext\n");
        assert!(buf.kill_to_end(), "on an empty tail it eats the break");
        assert_eq!(buf.text(), "keepnext\n");

        let mut words = TextBuf::from("foo.bar baz");
        words.end();
        assert!(words.word_backspace());
        assert_eq!(words.text(), "foo.bar ");
        words.word_backspace();
        assert_eq!(
            words.text(),
            "foo.",
            "the space then the word, in one press"
        );
        words.word_backspace();
        assert_eq!(words.text(), "foo", "punctuation is a word of its own");

        // Across a line break it is exactly the join `backspace` would have done.
        let mut joined = TextBuf::from("a\nb");
        joined.cursor = Pos { line: 1, col: 0 };
        assert!(joined.word_backspace());
        assert_eq!(joined.text(), "ab");
        let mut start = TextBuf::from("a");
        assert!(
            !start.word_backspace(),
            "nothing to delete at the very start"
        );
    }

    #[test]
    fn textbuf_arrows_keep_a_sticky_column() {
        let mut buf = TextBuf::from("aaaaaa\nbb\ncccccc\n");
        buf.cursor = Pos { line: 0, col: 5 };
        buf.down();
        assert_eq!(
            buf.cursor,
            Pos { line: 1, col: 2 },
            "clamped to the short line"
        );
        buf.down();
        assert_eq!(
            buf.cursor,
            Pos { line: 2, col: 5 },
            "and back to the column the user was aiming for"
        );
        buf.left();
        buf.up();
        assert_eq!(
            buf.cursor.line, 1,
            "a horizontal move drops the sticky column"
        );
        assert_eq!(buf.cursor.col, 2);

        // Left at column 0 wraps to the end of the line above; right does the mirror.
        buf.cursor = Pos { line: 1, col: 0 };
        buf.left();
        assert_eq!(buf.cursor, Pos { line: 0, col: 6 });
        buf.right();
        assert_eq!(buf.cursor, Pos { line: 1, col: 0 });
        buf.home();
        assert_eq!(buf.cursor.col, 0);
        buf.end();
        assert_eq!(buf.cursor.col, 2);

        // Paging is a vertical move by `rows`, clamped at both ends.
        buf.page_up(10);
        assert_eq!(buf.cursor.line, 0);
        buf.page_down(10);
        assert_eq!(buf.cursor.line, 2);
    }

    #[test]
    fn textbuf_word_motion_treats_punctuation_runs_as_words() {
        let mut buf = TextBuf::from("let x = foo.bar(1);\nnext\n");
        let mut stops = vec![buf.cursor.col];
        for _ in 0..9 {
            buf.word_right();
            if buf.cursor.line != 0 {
                break;
            }
            stops.push(buf.cursor.col);
        }
        assert_eq!(
            stops,
            vec![0, 3, 5, 7, 11, 12, 15, 16, 17, 19],
            "`let`, `x`, `=`, `foo`, `.`, `bar`, `(`, `1`, `);`"
        );
        buf.cursor = Pos { line: 0, col: 19 };
        buf.word_left();
        assert_eq!(buf.cursor.col, 17);
        buf.word_left();
        assert_eq!(buf.cursor.col, 16);

        // At either end of a line, word motion steps to the neighbouring line and stops.
        buf.cursor = Pos { line: 1, col: 0 };
        buf.word_left();
        assert_eq!(buf.cursor, Pos { line: 0, col: 19 });
        buf.end();
        buf.word_right();
        assert_eq!(buf.cursor, Pos { line: 1, col: 0 });
    }

    #[test]
    fn textbuf_wide_and_combining_chars_keep_columns_honest() {
        let buf = fixture();
        let line = &buf.lines[1].text; // "\tw=字 e\u{0301}q"
        assert_eq!(col_width(line, 1), TAB_STOP, "a tab reaches the next stop");
        assert_eq!(col_width(line, 3), TAB_STOP + 2, "up to the wide char");
        assert_eq!(col_width(line, 4), TAB_STOP + 4, "字 is two columns");
        // The combining acute adds no column of its own.
        let chars: Vec<char> = line.chars().collect();
        let e = chars.iter().position(|c| *c == 'e').expect("an e");
        assert_eq!(chars[e + 1], '\u{0301}');
        assert_eq!(col_width(line, e + 1), col_width(line, e + 2));

        // A tab that does not start at a stop takes only what is left of the run.
        assert_eq!(col_width("ab\tc", 3), TAB_STOP);
        assert_eq!(col_width("ab\tc", 4), TAB_STOP + 1);

        // The click maps a display column back to the char that owns it, never inside one.
        let mut buf = fixture();
        buf.cursor.line = 1;
        buf.click(1, TAB_STOP + 3);
        assert_eq!(
            col_width(&buf.lines[1].text, buf.cursor.col),
            TAB_STOP + 2,
            "a click on 字's second cell lands before it, never inside a char"
        );
        buf.click(1, TAB_STOP + 4);
        assert_eq!(
            col_width(&buf.lines[1].text, buf.cursor.col),
            TAB_STOP + 4,
            "and a click just past it lands just past it"
        );
    }

    #[test]
    fn textbuf_viewport_keeps_the_caret_visible_when_scrolling_both_ways() {
        let text: String = (1..=20)
            .map(|i| format!("line {i} {}\n", "x".repeat(40)))
            .collect();
        let mut buf = TextBuf::from(text.as_str());

        let view = buf.viewport(5, 20, Wrap::None);
        assert_eq!(view.rows.len(), 5);
        assert_eq!(view.caret, (0, 0));
        assert_eq!(view.first_line, 0);
        assert!(
            view.clipped.iter().all(|c| *c),
            "every line runs off the edge"
        );

        // Down past the bottom scrolls by one, not by a page.
        for _ in 0..6 {
            buf.down();
        }
        let view = buf.viewport(5, 20, Wrap::None);
        assert_eq!(buf.top, 2);
        assert_eq!(view.first_line, 2);
        assert_eq!(view.caret.0, 4, "the caret sits on the last row");
        assert!(view.rows[0].starts_with("line 3"));

        // End of a long line scrolls sideways; the caret stays inside the window.
        buf.end();
        let view = buf.viewport(5, 20, Wrap::None);
        assert!(buf.left > 0, "scrolled right: {}", buf.left);
        assert_eq!(view.caret.1, 19, "on the last column of the window");
        assert_eq!(
            view.rows[4].width(),
            19,
            "the caret sits one past the last character, which is not drawn"
        );

        // Home scrolls back, and a narrower window re-clamps on the next call (a Resize).
        buf.home();
        let view = buf.viewport(5, 8, Wrap::None);
        assert_eq!(buf.left, 0);
        assert_eq!(view.caret, (4, 0));
        assert_eq!(view.rows[0].width(), 8);

        // Up above the top scrolls the other way.
        for _ in 0..8 {
            buf.up();
        }
        buf.viewport(5, 8, Wrap::None);
        assert_eq!(buf.top, 0);
    }

    #[test]
    fn textbuf_viewport_soft_wraps_for_the_note_modal() {
        let mut buf = TextBuf::from("aaaaaaaaaa\nshort\n");
        let view = buf.viewport(5, 4, Wrap::Soft);
        assert_eq!(view.rows, vec!["aaaa", "aaaa", "aa", "shor", "t"]);
        assert_eq!(view.caret, (0, 0));
        assert_eq!(buf.left, 0, "soft wrap never scrolls sideways");
        assert!(
            !view.clipped.iter().any(|c| *c),
            "nothing falls off the edge"
        );

        // The caret on the last wrapped row scrolls the window by display rows.
        buf.cursor = Pos { line: 1, col: 5 };
        let view = buf.viewport(2, 4, Wrap::Soft);
        assert_eq!(view.rows, vec!["shor", "t"]);
        assert_eq!(view.caret, (1, 1));
        assert_eq!(view.first_line, 1);

        // A tab inside a wrapped line still renders as the spaces it covers.
        let mut tabbed = TextBuf::from("\tx\n");
        let view = tabbed.viewport(2, 20, Wrap::Soft);
        assert_eq!(view.rows, vec![format!("{}x", " ".repeat(TAB_STOP))]);
    }

    #[test]
    fn textbuf_open_at_a_line_and_position_label() {
        let buf = TextBuf::open("a\nb\nc\n", 2);
        assert_eq!(buf.cursor, Pos { line: 1, col: 0 });
        assert_eq!(buf.position_label(), "line 2/3");
        assert_eq!(
            TextBuf::open("a\n", 99).cursor.line,
            0,
            "a line past the end clamps"
        );
        assert_eq!(TextBuf::open("a\n", 0).cursor.line, 0);
        assert_eq!(row_width("字x"), 3);
    }

    /// Deliverable 4: the key map is the buffer's vocabulary and nothing else, and a
    /// keymap letter inside a buffer is text — the reason `q` does not quit while a modal
    /// is open.
    #[test]
    fn textbuf_apply_maps_every_edit_key() {
        use crossterm::event::{KeyCode, KeyModifiers};

        use crate::tui::input::{Key, edit_key};

        let key =
            |code, mods| Key::of(&crossterm::event::KeyEvent::new(code, mods)).expect("a key");
        let ctrl = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        let none = KeyModifiers::NONE;

        // The bindings deliverable 4 promises, each resolved through `edit_key`.
        for (code, mods, want) in [
            (KeyCode::Char('x'), none, EditKey::Insert("x".into())),
            (KeyCode::Char('q'), none, EditKey::Insert("q".into())),
            (KeyCode::Tab, none, EditKey::Insert("\t".into())),
            (KeyCode::Char('j'), ctrl, EditKey::Newline),
            (KeyCode::Enter, alt, EditKey::Newline),
            (KeyCode::Backspace, none, EditKey::Backspace),
            (KeyCode::Delete, none, EditKey::Delete),
            (KeyCode::Left, none, EditKey::Left),
            (KeyCode::Right, none, EditKey::Right),
            (KeyCode::Up, none, EditKey::Up),
            (KeyCode::Down, none, EditKey::Down),
            (KeyCode::Left, alt, EditKey::WordLeft),
            (KeyCode::Left, ctrl, EditKey::WordLeft),
            (KeyCode::Right, alt, EditKey::WordRight),
            (KeyCode::Right, ctrl, EditKey::WordRight),
            (KeyCode::Backspace, alt, EditKey::WordBackspace),
            (KeyCode::Char('w'), ctrl, EditKey::WordBackspace),
            (KeyCode::Home, none, EditKey::Home),
            (KeyCode::End, none, EditKey::End),
            (KeyCode::Char('a'), ctrl, EditKey::Home),
            (KeyCode::Char('e'), ctrl, EditKey::End),
            (KeyCode::Char('k'), ctrl, EditKey::KillToEnd),
            (KeyCode::PageUp, none, EditKey::PageUp),
            (KeyCode::PageDown, none, EditKey::PageDown),
        ] {
            assert_eq!(
                edit_key(&key(code, mods), false),
                Some(want),
                "{code:?} + {mods:?}"
            );
        }

        // Enter and Esc belong to whoever holds the buffer, and Shift-Enter is a newline
        // only where the terminal can tell it from Enter.
        assert_eq!(edit_key(&key(KeyCode::Enter, none), true), None);
        assert_eq!(edit_key(&key(KeyCode::Esc, none), true), None);
        assert_eq!(edit_key(&key(KeyCode::Char('c'), ctrl), false), None);
        assert_eq!(
            edit_key(&key(KeyCode::Enter, KeyModifiers::SHIFT), false),
            None,
            "with no enhancement Shift-Enter is Enter, and Enter is not ours"
        );
        assert_eq!(
            edit_key(&key(KeyCode::Enter, KeyModifiers::SHIFT), true),
            Some(EditKey::Newline)
        );

        // And `apply` performs them, reporting whether the frame changed.
        let mut buf = TextBuf::from("ab\ncd\n");
        assert!(buf.apply(EditKey::End, 5));
        assert!(buf.apply(EditKey::Insert("Z".into()), 5));
        assert_eq!(buf.text(), "abZ\ncd\n");
        assert!(buf.apply(EditKey::PageDown, 5));
        assert_eq!(buf.cursor.line, 1);
        assert!(!buf.apply(EditKey::PageDown, 5), "already at the last line");
        assert!(buf.apply(EditKey::Home, 5));
        assert!(buf.apply(EditKey::Backspace, 5), "column 0 joins the lines");
        assert_eq!(buf.text(), "abZcd\n");
        assert!(buf.apply(EditKey::Home, 5));
        assert!(
            !buf.apply(EditKey::PageUp, 5),
            "a page move with nowhere to go is not a frame"
        );
        assert!(
            !buf.apply(EditKey::Backspace, 5),
            "nothing before the start of the buffer"
        );
    }

    mod proptests {
        use proptest::prelude::*;

        use super::*;

        /// 8 cases in the unit tier, `PROPTEST_CASES` (64 from the pre-push hook) when set —
        /// the same rule the engine's store-backed proptests follow.
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

        /// Text built out of the pieces that break naive line splitting: CRLF, a lone `\r`,
        /// a BOM, tabs, NUL, wide and combining characters, and an empty tail.
        fn any_text() -> impl Strategy<Value = String> {
            let piece = prop_oneof![
                Just("a".to_owned()),
                Just("\n".to_owned()),
                Just("\r\n".to_owned()),
                Just("\r".to_owned()),
                Just("\t".to_owned()),
                Just("\u{FEFF}".to_owned()),
                Just("\0".to_owned()),
                Just("字".to_owned()),
                Just("e\u{0301}".to_owned()),
                Just(" ".to_owned()),
            ];
            prop::collection::vec(piece, 0..24).prop_map(|v| v.concat())
        }

        /// **The rule of the whole module.** Parsing and printing is the identity, and an
        /// edit-then-undo is too: whatever an editor session does to bytes it did not touch,
        /// it must be nothing.
        #[test]
        fn textbuf_round_trips_any_utf8_text() {
            proptest!(config(), |(text in any_text())| {
                let buf = TextBuf::from(text.as_str());
                prop_assert_eq!(buf.text(), text.clone());

                // And the buffer is well formed however it was parsed.
                prop_assert!(!buf.lines.is_empty());
                for line in &buf.lines {
                    prop_assert!(!line.text.contains('\n'), "a newline never lives inside a line");
                }

                // A char typed at the end and taken back leaves exactly what came in.
                // (It lands *before* a trailing newline, as it does in vim: a terminated
                // file has no position after its last terminator.)
                let mut edited = TextBuf::from(text.as_str());
                edited.cursor.line = edited.lines.len() - 1;
                edited.end();
                edited.insert_char('Z');
                prop_assert_eq!(edited.text().len(), text.len() + 1);
                prop_assert!(edited.text().contains('Z'));
                prop_assert!(edited.backspace());
                prop_assert_eq!(edited.text(), text.clone());

                // So does a newline split at the cursor and the join that undoes it.
                let mut split = TextBuf::from(text.as_str());
                split.newline();
                prop_assert!(split.backspace());
                prop_assert_eq!(split.text(), text);
            });
        }
    }
}
