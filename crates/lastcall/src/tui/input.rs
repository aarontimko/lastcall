//! The input vocabulary (kickoff deliverable 5): the [`Action`] enum and the
//! [`DEFAULT_KEYMAP`] table (worker 3a), plus the crossterm translation
//! [`to_action`]`(Event, &Keymap)` and [`Keymap::from_config`], the `[keys]` override with
//! its three error classes (worker 3b).
//!
//! Key spec grammar: optional `ctrl-` / `alt-` / `shift-` prefixes, then a single character
//! or a named key (`up down left right pageup pagedown home end enter esc tab backtab space
//! backspace delete f1..f12`). Specs are case-insensitive (`Ctrl-C` = `ctrl-c`, `K` = `k`);
//! an upper-case letter is spelled `shift-k`, and `shift-tab` is `backtab`. Matching is
//! exact after that normalization, on both the spec and the terminal's key event
//! ([`Key::parse`] and [`Key::of`] apply the same folding).

use std::collections::BTreeMap;
use std::fmt;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use lastcall_engine::config::KeySpecs;

use super::herdr::HerdrUpdate;

/// Diff lines one wheel notch scrolls.
pub const WHEEL_LINES: u16 = 3;

/// Everything the app can be asked to do. Keys, mouse gestures and the tick all become one
/// of these before they touch [`super::app::App`], so the keyboard and mouse paths are
/// provably equivalent (the parity tests) and the reducer never sees a crossterm type.
///
/// Not `Copy`: `Herdr` carries the derived association (deliverable 4), which owns strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nav focus: previous entry. Diff focus: scroll up one line.
    NavUp,
    /// Nav focus: next entry. Diff focus: scroll down one line.
    NavDown,
    /// Nav focus: a page of entries up. Diff focus: a page of lines up.
    NavPageUp,
    /// Nav focus: a page of entries down. Diff focus: a page of lines down.
    NavPageDown,
    /// Focus the diff for the selected row/group (the cursor stays on that file's current
    /// hunk); on a root entry, select its first row. Bound to `enter`, `l` and `right`.
    Open,
    /// Close the help overlay if open, else return focus to the nav with the same row
    /// selected. Never quits. Bound to `esc`, `h` and `left`.
    Back,
    FocusToggle,
    HunkNext,
    HunkPrev,
    /// Ask the engine for the hunks of the selected **collapsed** row (`Effect::Expand`).
    /// A no-op — no effect, no draw — on a binary row and on any row that is not collapsed
    /// (Phase 6 deliverable 4).
    Expand,
    /// Scroll the diff up `n` lines (the wheel; the loop sends `NavUp` when the pointer is
    /// over the nav instead).
    ScrollUp(u16),
    ScrollDown(u16),
    ToggleFullPaths,
    ToggleRemote,
    /// `t`: flip [`crate::tui::app::App::hide_empty`] — the nav either lists every repo in
    /// scope (the default) or drops the ones with nothing pending and no agent flag
    /// (§6.7, Amendment v1.9 item 4).
    HideEmpty,
    /// Ask the loop for a rescan (`Effect::Refresh`); ignored while one is running.
    Refresh,
    Help,
    Quit,
    /// Mouse press at (column, row); the loop resolves it through the last `HitMap` and calls
    /// `App::hit`, so the reducer itself treats `Press` as a no-op.
    Press(u16, u16),
    /// Pointer moved with the button held (only the divider drag uses it).
    Drag(u16, u16),
    Release,
    Resize(u16, u16),
    /// One second passed (the loop's 1 s timer): status-line ages advance.
    Tick,
    /// Accept what the cursor is on (§6.7): on a file row with hunks, the one hunk under
    /// the diff cursor whichever pane has focus; else the selected entry — a hunkless row
    /// whole, a group, every row of a root.
    Accept,
    /// Accept the selected row whole, whichever pane has focus.
    AcceptFile,
    /// Accept every row of every listed root.
    AcceptAll,
    /// Put back what the cursor is on (§6.3): on a row with content hunks in the diff, the
    /// one hunk under the cursor; else the selected row whole. A deletion row's single
    /// hunk *is* the file, so `u` on it restores the file (kickoff F16).
    Restore,
    /// Restore the selected row whole, whichever pane has focus.
    RestoreFile,
    /// Open the note modal on the hunk under the cursor (a file, from the nav).
    Flag,
    /// Clear every flag on the selected file.
    Unflag,
    /// Open the selected file in the **inline** editor, at the line of the hunk under the
    /// cursor (deliverable 8; `Effect::EditInline`). The same rows `EditExternal` opens,
    /// and only when the engine will hand the bytes over: a binary or oversize file says
    /// `use shift-i: <why>` instead.
    Edit,
    /// Suspend the TUI and open the selected file in `$VISUAL`/`$EDITOR`, at the line of
    /// the hunk under the cursor (deliverable 7; `Effect::EditExternal`). A row with
    /// nothing to open — a deletion, a symlink, a path that is not UTF-8 — says
    /// `not editable` instead.
    EditExternal,
    /// One edit inside the note modal. Never key-bound: while the modal is open every key
    /// is resolved by [`note_action`] before the keymap is consulted.
    Note(NoteKey),
    /// One keystroke inside the inline editor. Never key-bound, for the same reason:
    /// [`editor_action`] resolves everything while `App::editor` is open, so a keymap
    /// letter — `q` included — types itself (F16).
    Editor(EditorKey),
    /// Start a line selection in the diff at the cursor line, or extend the one that is
    /// already running (deliverable 9). Whole lines only: the unit the reader is reviewing
    /// in is a diff line, and a half-line copied out of a `-` and a `+` is not a thing
    /// anyone means to paste.
    Select,
    /// Copy the selection — or, with none, the hunk under the cursor — to the system
    /// clipboard through OSC 52 (`Effect::Copy`), and clear the selection.
    Copy,
    /// Extend a **mouse** selection to diff line `n` (absolute, not a screen row). Built by
    /// the loop from a `Drag`, which is the only place the pane's rectangle is known; the
    /// reducer ignores it unless a press landed in the diff body first, which is how a
    /// divider drag stays a divider drag (design review F13).
    SelectTo(usize),
    /// One move inside the agent picker. Never key-bound, for the same reason.
    Pick(PickKey),
    /// Answer the confirm modal (`y` / `Enter`); nothing outside it.
    Confirm,
    /// Dismiss the confirm modal (`n` / `Esc`); nothing outside it.
    Cancel,
    /// Ack the selected root's herdr flag (deliverable 5). Local to this process.
    Ack,
    /// Focus the selected root's agent in herdr (`Effect::Focus`).
    Jump,
    /// Toggle the herdr workspace scope for this session (deliverable 8).
    ScopeToggle,
    /// News from the herdr task. Never key-bound; the loop's fourth `select!` arm makes it.
    Herdr(HerdrUpdate),
}

/// What one keystroke does to the note being typed.
///
/// The editing half is [`EditKey`], the vocabulary the note modal shares with the inline
/// editor (deliverable 5): everything the buffer knows how to do, including a bracketed
/// paste, which arrives as one `Event::Paste` and must land as one edit — never as a send,
/// whatever it contains. `Send` and `Cancel` are the modal's own, because only the modal
/// knows what ends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteKey {
    /// Anything the note's text buffer does with the keystroke.
    Edit(EditKey),
    /// Enter: write the flag and close the modal.
    Send,
    /// Esc: close the modal, write nothing.
    Cancel,
}

/// One edit inside a text buffer ([`super::textbuf::TextBuf`]), for the two places that
/// hold one: the note modal (deliverable 5) and the inline editor (deliverable 8).
///
/// Deliberately *only* the buffer's own vocabulary. `Send`, `Cancel`, `Save` and a paste's
/// destination are the caller's business — the note modal sends a flag where the editor
/// writes a file — so they stay out of here and the same mapping serves both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditKey {
    /// Text to put in verbatim. A `String`, not a `char`, because a bracketed paste arrives
    /// as one `Event::Paste` and must land as one edit, whatever it contains.
    Insert(String),
    Newline,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    WordLeft,
    WordRight,
    WordBackspace,
    Home,
    End,
    /// `Ctrl-K`: from the cursor to the end of the line.
    KillToEnd,
    PageUp,
    PageDown,
}

/// The buffer edit one key event asks for, or nothing (Phase 8 deliverable 4).
///
/// Everything a terminal reports for the motions people expect in a text field, and nothing
/// else: `Enter` and `Esc` are **not** here, because what they mean depends on who is
/// holding the buffer — the note modal sends and cancels, the inline editor saves and
/// closes. The caller checks those first and asks this second.
///
/// `Shift-Enter` is a newline only when the terminal can tell it apart from `Enter`, which
/// is why `enhanced` is a parameter: with no keyboard-enhancement flags the two are the
/// same byte, and promising a key that silently sends the note instead is worse than not
/// promising it (deliverable 5). `Ctrl-J` is the binding that always works.
///
/// `Ctrl-Enter` is a newline too (verifier (a) F5): every chat UI the reviewer came from
/// treats it as one, and it is only ever distinguishable from `Enter` under the same kitty
/// protocol `Shift-Enter` needs — so where it can be told apart it breaks the line, and
/// where it cannot it is a plain `Enter` and sends, which is the honest default.
///
/// `Ctrl-H` is `Backspace` (verifier (a) F4): crossterm parses the byte `0x08` as
/// `Char('h') + CONTROL` — only `0x7f` is `KeyCode::Backspace` — so a terminal configured
/// to send `^H` for its Backspace key (xterm `backarrowKey`, some tmux and urxvt setups)
/// would otherwise lose Backspace entirely.
pub fn edit_key(key: &Key, enhanced: bool) -> Option<EditKey> {
    let alt_or_ctrl = key.alt || key.ctrl;
    Some(match key.code {
        KeyCode::Enter if key.alt || key.ctrl => EditKey::Newline,
        KeyCode::Enter if key.shift && enhanced => EditKey::Newline,
        KeyCode::Char('j') if key.ctrl => EditKey::Newline,
        KeyCode::Backspace if key.alt || key.ctrl => EditKey::WordBackspace,
        KeyCode::Char('w') if key.ctrl => EditKey::WordBackspace,
        KeyCode::Char('h') if key.ctrl => EditKey::Backspace,
        KeyCode::Backspace => EditKey::Backspace,
        KeyCode::Delete => EditKey::Delete,
        KeyCode::Left if alt_or_ctrl => EditKey::WordLeft,
        KeyCode::Right if alt_or_ctrl => EditKey::WordRight,
        KeyCode::Left => EditKey::Left,
        KeyCode::Right => EditKey::Right,
        KeyCode::Up => EditKey::Up,
        KeyCode::Down => EditKey::Down,
        KeyCode::Home => EditKey::Home,
        KeyCode::End => EditKey::End,
        KeyCode::Char('a') if key.ctrl => EditKey::Home,
        KeyCode::Char('e') if key.ctrl => EditKey::End,
        KeyCode::Char('k') if key.ctrl => EditKey::KillToEnd,
        KeyCode::PageUp => EditKey::PageUp,
        KeyCode::PageDown => EditKey::PageDown,
        KeyCode::Tab if !alt_or_ctrl => EditKey::Insert("\t".to_owned()),
        // A printable is text. That is what keeps a keymap letter — `q`, `a`, `i` — from
        // being swallowed by the buffer while a modal is open: it types itself.
        KeyCode::Char(c) if !alt_or_ctrl => EditKey::Insert(c.to_string()),
        _ => return None,
    })
}

/// What one keystroke does in the agent picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickKey {
    Up,
    Down,
    /// Enter: stage the export into the selected pane.
    Send,
    /// Esc: leave the flag written and stage nothing.
    Cancel,
}

/// What one keystroke does inside the inline editor (deliverable 8).
///
/// The same shape as [`NoteKey`] — the buffer's own vocabulary plus the two keys only the
/// holder can decide — with the editor's answers instead of the modal's: `Ctrl-S` writes
/// the file, `Esc` closes it (asking first when the buffer is dirty). The mouse is here
/// too, because the editor is the one place a click means "put the caret there".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorKey {
    /// Anything the buffer does with the keystroke.
    Edit(EditKey),
    /// `Ctrl-S`: write the buffer back through the engine's CAS'd save.
    Save,
    /// `Esc`: close a clean buffer, ask about a dirty one.
    Close,
    /// A click at this **pane-relative** row and column of the text area (the gutter is
    /// already subtracted): the reducer adds the buffer's own scroll.
    Click(u16, u16),
    /// The wheel: `+n` down, `-n` up.
    Scroll(i32),
}

/// The inline editor's action for one key event, consulted before the keymap while
/// `App::editor` is open (deliverable 8; F16).
///
/// `Esc` and `Ctrl-S` are the editor's own two keys, checked first; `Enter` is a newline,
/// not a send, because this is a file and not a message. Everything else [`edit_key`]
/// answers, so every keymap letter types itself — the reason `q` cannot quit from inside
/// the editor, and the reason a **non-printable** `quit` binding (`ctrl-c` by default)
/// still can, exactly as it does under the note modal.
pub fn editor_action(event: &Event, keymap: &Keymap, enhanced: bool) -> Option<Action> {
    if let Event::Paste(text) = event {
        return Some(Action::Editor(EditorKey::Edit(EditKey::Insert(
            text.clone(),
        ))));
    }
    let Event::Key(k) = event else {
        return None;
    };
    let key = Key::of(k)?;
    if key.code == KeyCode::Esc {
        return Some(Action::Editor(EditorKey::Close));
    }
    if key.code == KeyCode::Char('s') && key.ctrl && !key.alt {
        return Some(Action::Editor(EditorKey::Save));
    }
    // Before `edit_key`, which only makes a newline of an `Enter` that carries a modifier:
    // in a file every `Enter` breaks the line.
    if key.code == KeyCode::Enter && !key.ctrl && !key.alt {
        return Some(Action::Editor(EditorKey::Edit(EditKey::Newline)));
    }
    if let Some(edit) = edit_key(&key, enhanced) {
        return Some(Action::Editor(EditorKey::Edit(edit)));
    }
    quit_only(keymap, key)
}

/// The note modal's action for one key event, consulted before the keymap while
/// `App.note` is open (deliverable 10).
///
/// Esc cancels and Enter sends — the modal's own two keys, checked first and last. In
/// between, [`edit_key`] answers: every printable key types, `ctrl-j` breaks the line, and
/// so do `alt-Enter` and — **only when `enhanced`** — `shift-Enter`, because with no
/// keyboard-enhancement flags Shift-Enter is byte-identical to Enter and promising it would
/// mean sending the note where the reviewer asked for a second line (deliverable 5).
/// Everything else is swallowed, except a **non-printable** `quit` binding (`ctrl-c` by
/// default), which quits as it does under the confirm modal — a printable one (`q`) types
/// its letter, because a note is text.
pub fn note_action(event: &Event, keymap: &Keymap, enhanced: bool) -> Option<Action> {
    if let Event::Paste(text) = event {
        return Some(Action::Note(NoteKey::Edit(EditKey::Insert(text.clone()))));
    }
    let Event::Key(k) = event else {
        return None;
    };
    let key = Key::of(k)?;
    if key.code == KeyCode::Esc {
        return Some(Action::Note(NoteKey::Cancel));
    }
    // The buffer gets first refusal on everything but Esc, so `alt-Enter` and an enhanced
    // `shift-Enter` are newlines before a bare Enter can be a send.
    if let Some(edit) = edit_key(&key, enhanced) {
        return Some(Action::Note(NoteKey::Edit(edit)));
    }
    if key.code == KeyCode::Enter {
        return Some(Action::Note(NoteKey::Send));
    }
    quit_only(keymap, key)
}

/// The picker's action for one key event, on the same terms as [`note_action`]. The picker
/// shows a list, so `j`/`k` move it as they do in the nav.
pub fn pick_action(event: &Event, keymap: &Keymap) -> Option<Action> {
    let Event::Key(k) = event else {
        return None;
    };
    let key = Key::of(k)?;
    let pick = match key.code {
        KeyCode::Up => PickKey::Up,
        KeyCode::Down => PickKey::Down,
        KeyCode::Char('k') if !key.ctrl && !key.alt => PickKey::Up,
        KeyCode::Char('j') if !key.ctrl && !key.alt => PickKey::Down,
        KeyCode::Enter => PickKey::Send,
        KeyCode::Esc => PickKey::Cancel,
        _ => return quit_only(keymap, key),
    };
    Some(Action::Pick(pick))
}

/// `Action::Quit` when `key` is a **non-printable** quit binding, else nothing: the escape
/// hatch a text-entry modal keeps open.
fn quit_only(keymap: &Keymap, key: Key) -> Option<Action> {
    let printable = matches!(key.code, KeyCode::Char(_)) && !key.ctrl && !key.alt;
    match keymap.lookup(key) {
        Some(Action::Quit) if !printable => Some(Action::Quit),
        _ => None,
    }
}

/// Action name (the `[keys]` config key) → default key specs, in help-overlay order.
pub const DEFAULT_KEYMAP: &[(&str, &[&str])] = &[
    ("nav_up", &["up", "k"]),
    ("nav_down", &["down", "j"]),
    ("nav_page_up", &["pageup", "b"]),
    ("nav_page_down", &["pagedown", "space"]),
    ("open", &["enter", "l", "right"]),
    ("back", &["esc", "h", "left"]),
    ("focus_toggle", &["tab"]),
    ("hunk_next", &["n", "]"]),
    ("hunk_prev", &["p", "["]),
    ("expand", &["e"]),
    ("toggle_full_paths", &["f"]),
    ("toggle_remote", &["o"]),
    ("hide_empty", &["t"]),
    ("accept", &["a"]),
    ("accept_file", &["shift-a"]),
    ("accept_all", &["ctrl-a"]),
    ("restore", &["u"]),
    ("restore_file", &["shift-u"]),
    ("flag", &["m"]),
    ("unflag", &["shift-m"]),
    ("select", &["v"]),
    ("copy", &["y"]),
    ("edit", &["i"]),
    ("edit_external", &["shift-i"]),
    ("ack", &["d"]),
    ("jump", &["g"]),
    ("scope", &["w"]),
    ("refresh", &["r"]),
    ("help", &["?"]),
    ("quit", &["q", "ctrl-c"]),
];

/// The confirm modal's keys, consulted before the keymap while `App::confirm` is open and
/// nowhere else. Not rebindable in v1 (kickoff deliverable 4), so they live outside
/// [`DEFAULT_KEYMAP`]; the help overlay appends them after the bindable rows.
pub const MODAL_KEYS: &[(&str, &[&str])] =
    &[("confirm", &["y", "enter"]), ("cancel", &["n", "esc"])];

/// The modal action for a key while the confirm is open: `Confirm`, `Cancel`, or nothing
/// (the loop then lets only the keymap's `quit` keys through and swallows every other
/// key, like the help overlay does).
pub fn modal_action(key: Key) -> Option<Action> {
    for (name, specs) in MODAL_KEYS {
        if specs.iter().any(|s| Key::parse(s).ok() == Some(key)) {
            return Some(match *name {
                "confirm" => Action::Confirm,
                _ => Action::Cancel,
            });
        }
    }
    None
}

impl Action {
    /// The key-bindable action for a `[keys]` name (`nav_up`, `quit`, …). `scroll_up` /
    /// `scroll_down` are accepted as one-line scrolls though unbound by default. Mouse,
    /// resize and tick actions carry payloads and are never key-bound.
    pub fn from_name(name: &str) -> Option<Action> {
        Some(match name {
            "nav_up" => Action::NavUp,
            "nav_down" => Action::NavDown,
            "nav_page_up" => Action::NavPageUp,
            "nav_page_down" => Action::NavPageDown,
            "open" => Action::Open,
            "back" => Action::Back,
            "focus_toggle" => Action::FocusToggle,
            "hunk_next" => Action::HunkNext,
            "hunk_prev" => Action::HunkPrev,
            "scroll_up" => Action::ScrollUp(1),
            "scroll_down" => Action::ScrollDown(1),
            "expand" => Action::Expand,
            "toggle_full_paths" => Action::ToggleFullPaths,
            "toggle_remote" => Action::ToggleRemote,
            "hide_empty" => Action::HideEmpty,
            "accept" => Action::Accept,
            "accept_file" => Action::AcceptFile,
            "accept_all" => Action::AcceptAll,
            "restore" => Action::Restore,
            "restore_file" => Action::RestoreFile,
            "flag" => Action::Flag,
            "unflag" => Action::Unflag,
            "select" => Action::Select,
            "copy" => Action::Copy,
            "edit" => Action::Edit,
            "edit_external" => Action::EditExternal,
            "ack" => Action::Ack,
            "jump" => Action::Jump,
            "scope" => Action::ScopeToggle,
            "refresh" => Action::Refresh,
            "help" => Action::Help,
            "quit" => Action::Quit,
            _ => return None,
        })
    }

    /// The help overlay's label for a keymap entry.
    pub fn describe(name: &str) -> &'static str {
        match name {
            "nav_up" => "previous entry / scroll up",
            "nav_down" => "next entry / scroll down",
            "nav_page_up" => "page up",
            "nav_page_down" => "page down",
            "open" => "open the diff",
            "back" => "back to the file list (close help)",
            "focus_toggle" => "toggle focus",
            "hunk_next" => "next hunk",
            "hunk_prev" => "previous hunk",
            "expand" => "expand a collapsed file",
            "toggle_full_paths" => "full paths",
            "toggle_remote" => "show org/repo",
            // 23 columns: the overlay caps a description at 30 (design review F15).
            "hide_empty" => "hide / show empty repos",
            "accept" => "accept the hunk or the selected entry",
            "accept_file" => "accept the whole file",
            "accept_all" => "accept everything listed",
            "restore" => "restore the hunk",
            "restore_file" => "restore the whole file",
            "flag" => "flag it with a note",
            "unflag" => "clear the file's flags",
            // Deliverable 7 wrote this description out in full ("open the file in $EDITOR at
            // the hunk under the cursor"); design review F15 and deliverable 11 cap every
            // help row at 30 columns and name this one `$EDITOR at the hunk`, because the
            // help overlay at 100×30 falls back to one clipped column the moment the two
            // widest rows plus 7 exceed the width. The overlay is the only place a reader
            // ever sees a description, so the short form is the one that survives.
            "select" => "select lines",
            "copy" => "copy selection",
            "edit" => "edit in place",
            "edit_external" => "$EDITOR at the hunk",
            "ack" => "ack the agent flag",
            "jump" => "jump to the agent in herdr",
            "scope" => "workspace scope on/off",
            "refresh" => "rescan now",
            "help" => "this help",
            "quit" => "quit",
            "scroll_up" => "scroll the diff up",
            "scroll_down" => "scroll the diff down",
            "confirm" => "confirm",
            "cancel" => "cancel",
            _ => "",
        }
    }
}

// ---- key specs -------------------------------------------------------------------------

/// A key as both a parsed spec and a normalized terminal event: the crossterm code plus the
/// modifiers that must be held. `shift` is only meaningful on named keys — for a character
/// the glyph carries it (`shift-k` is `Char('K')`), and `shift-tab` is `BackTab`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

/// The named keys of the spec grammar.
pub const NAMED_KEYS: &[(&str, KeyCode)] = &[
    ("up", KeyCode::Up),
    ("down", KeyCode::Down),
    ("left", KeyCode::Left),
    ("right", KeyCode::Right),
    ("pageup", KeyCode::PageUp),
    ("pagedown", KeyCode::PageDown),
    ("home", KeyCode::Home),
    ("end", KeyCode::End),
    ("enter", KeyCode::Enter),
    ("esc", KeyCode::Esc),
    ("tab", KeyCode::Tab),
    ("backtab", KeyCode::BackTab),
    ("space", KeyCode::Char(' ')),
    ("backspace", KeyCode::Backspace),
    ("delete", KeyCode::Delete),
];

/// Fold the shift modifier into the code: a character's glyph carries it (`shift-k` is
/// `Char('K')`), a ctrl chord ignores it and is case-insensitive (`ctrl-c` however the
/// terminal spells it), `shift-tab` is `BackTab`; only a named key keeps `shift` as a flag.
fn fold_shift(code: KeyCode, ctrl: bool, shift: bool) -> (KeyCode, bool) {
    match code {
        KeyCode::Char(c) if ctrl => (KeyCode::Char(c.to_lowercase().next().unwrap_or(c)), false),
        KeyCode::Char(c) if shift => (KeyCode::Char(c.to_uppercase().next().unwrap_or(c)), false),
        KeyCode::Char(_) => (code, false),
        KeyCode::Tab if shift => (KeyCode::BackTab, false),
        KeyCode::BackTab => (KeyCode::BackTab, false),
        other => (other, shift),
    }
}

impl Key {
    /// Parse one key spec (see the module docs for the grammar). The error is the reason,
    /// without the spec itself; [`KeymapError::BadSpec`] adds the action and the spec.
    pub fn parse(spec: &str) -> Result<Key, String> {
        let lower = spec.trim().to_lowercase();
        if lower.is_empty() {
            return Err("empty key spec".to_owned());
        }
        let (mut ctrl, mut alt, mut shift) = (false, false, false);
        let mut rest = lower.as_str();
        while let Some((head, tail)) = rest.split_once('-') {
            if tail.is_empty() {
                break;
            }
            let flag = match head {
                "ctrl" => &mut ctrl,
                "alt" => &mut alt,
                "shift" => &mut shift,
                _ => break,
            };
            if *flag {
                return Err(format!("`{head}-` given twice"));
            }
            *flag = true;
            rest = tail;
        }
        let named = NAMED_KEYS.iter().find(|(n, _)| *n == rest).map(|(_, c)| *c);
        let code = match named {
            Some(code) => code,
            None => {
                let fkey = rest
                    .strip_prefix('f')
                    .and_then(|n| n.parse::<u8>().ok())
                    .filter(|n| (1..=12).contains(n));
                let mut chars = rest.chars();
                match (fkey, chars.next(), chars.next()) {
                    (Some(n), _, _) => KeyCode::F(n),
                    (None, Some(c), None) => KeyCode::Char(c),
                    _ => {
                        return Err(format!(
                            "`{rest}` is not a single character, f1..f12 or one of {}",
                            NAMED_KEYS
                                .iter()
                                .map(|(n, _)| *n)
                                .collect::<Vec<_>>()
                                .join(" ")
                        ));
                    }
                }
            }
        };
        let (code, shift) = fold_shift(code, ctrl, shift);
        Ok(Key {
            code,
            ctrl,
            alt,
            shift,
        })
    }

    /// The canonical spelling of this key in the spec grammar: what [`Key::parse`] folded
    /// (`Q` → `q`, `shift-k` → `K`, `ctrl-C` → `ctrl-c`, `shift-tab` → `backtab`), so the
    /// help overlay shows the key that is bound, not the spec as typed.
    pub fn spec(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("ctrl-");
        }
        if self.alt {
            s.push_str("alt-");
        }
        if self.shift {
            s.push_str("shift-");
        }
        match self.code {
            KeyCode::Char(' ') => s.push_str("space"),
            KeyCode::Char(c) => s.push(c),
            KeyCode::F(n) => s.push_str(&format!("f{n}")),
            code => s.push_str(
                NAMED_KEYS
                    .iter()
                    .find(|(_, c)| *c == code)
                    .map(|(n, _)| *n)
                    .unwrap_or("?"),
            ),
        }
        s
    }

    /// Normalize a terminal key event the way [`Key::parse`] normalizes a spec. `None` for
    /// a key release (terminals with the enhanced keyboard protocol report those).
    pub fn of(event: &KeyEvent) -> Option<Key> {
        if event.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
        let alt = event.modifiers.contains(KeyModifiers::ALT);
        let shift = event.modifiers.contains(KeyModifiers::SHIFT);
        let (code, shift) = fold_shift(event.code, ctrl, shift);
        Some(Key {
            code,
            ctrl,
            alt,
            shift,
        })
    }
}

// ---- keymap ----------------------------------------------------------------------------

/// Why a `[keys]` table was refused. `lastcall tui` prints it and exits 2 before raw mode;
/// `lastcall config` prints it and exits 2 so a bad table is visible headlessly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeymapError {
    /// The table names an action that does not exist.
    UnknownAction { action: String },
    /// A spec does not follow the grammar (or an action was given no key at all).
    BadSpec {
        action: String,
        spec: String,
        reason: String,
    },
    /// After the overrides replaced their defaults, one key means two things.
    Duplicate {
        spec: String,
        first: String,
        second: String,
    },
}

impl fmt::Display for KeymapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeymapError::UnknownAction { action } => write!(
                f,
                "[keys] unknown action `{action}` (actions: {})",
                DEFAULT_KEYMAP
                    .iter()
                    .map(|(n, _)| *n)
                    .chain(["scroll_up", "scroll_down"])
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            KeymapError::BadSpec {
                action,
                spec,
                reason,
            } => write!(f, "[keys] {action}: bad key spec \"{spec}\": {reason}"),
            KeymapError::Duplicate {
                spec,
                first,
                second,
            } => write!(
                f,
                "[keys] \"{spec}\" is bound to both `{first}` and `{second}`"
            ),
        }
    }
}

impl std::error::Error for KeymapError {}

/// The effective bindings: [`DEFAULT_KEYMAP`] with each `[keys]` entry replacing that
/// action's defaults, parsed once. `table()` is what `App.keymap` shows in the hint line
/// and the help overlay; `lookup` is what [`to_action`] resolves a key event through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    table: Vec<(String, Vec<String>)>,
    bindings: Vec<(Key, Action)>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::defaults()
    }
}

impl Keymap {
    /// The default bindings alone.
    pub fn defaults() -> Keymap {
        Self::from_config(&BTreeMap::new()).expect("DEFAULT_KEYMAP parses")
    }

    /// Merge a `[keys]` table over the defaults. An entry replaces that action's default
    /// specs (it never appends); `scroll_up` / `scroll_down` may be bound though unbound by
    /// default. Errors, in order of detection: an unknown action name, an unparsable (or
    /// missing) spec, one key bound to two actions after the merge.
    pub fn from_config(keys: &BTreeMap<String, KeySpecs>) -> Result<Keymap, KeymapError> {
        let mut table: Vec<(String, Vec<String>)> = DEFAULT_KEYMAP
            .iter()
            .map(|(name, specs)| {
                (
                    (*name).to_owned(),
                    specs.iter().map(|s| (*s).to_owned()).collect(),
                )
            })
            .collect();
        for (name, specs) in keys {
            // `confirm` / `cancel` are not in `from_name`, so `[keys]` refuses them too.
            if Action::from_name(name).is_none() {
                return Err(KeymapError::UnknownAction {
                    action: name.clone(),
                });
            }
            let specs: Vec<String> = specs.specs().into_iter().map(str::to_owned).collect();
            if specs.is_empty() {
                return Err(KeymapError::BadSpec {
                    action: name.clone(),
                    spec: String::new(),
                    reason: "no key given (a list must name at least one key)".to_owned(),
                });
            }
            match table.iter_mut().find(|(n, _)| n == name) {
                Some(entry) => entry.1 = specs,
                None => table.push((name.clone(), specs)),
            }
        }
        let mut bindings: Vec<(Key, Action)> = Vec::new();
        let mut owners: Vec<(Key, String)> = Vec::new();
        for (name, specs) in &mut table {
            let action = Action::from_name(name).expect("every table entry was validated");
            for spec in specs.iter_mut() {
                let key = Key::parse(spec).map_err(|reason| KeymapError::BadSpec {
                    action: name.clone(),
                    spec: spec.clone(),
                    reason,
                })?;
                *spec = key.spec(); // the table shows the canonical spelling
                if let Some((_, first)) = owners.iter().find(|(k, _)| *k == key) {
                    if first != name {
                        return Err(KeymapError::Duplicate {
                            spec: spec.clone(),
                            first: first.clone(),
                            second: name.clone(),
                        });
                    }
                    continue; // the same key listed twice for one action is harmless
                }
                owners.push((key, name.clone()));
                bindings.push((key, action.clone()));
            }
        }
        Ok(Keymap { table, bindings })
    }

    /// `(action name, key specs)` in help-overlay order, overrides applied: what goes into
    /// `App.keymap`.
    pub fn table(&self) -> Vec<(String, Vec<String>)> {
        self.table.clone()
    }

    /// Every parsed binding.
    pub fn bindings(&self) -> &[(Key, Action)] {
        &self.bindings
    }

    pub fn lookup(&self, key: Key) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, a)| a.clone())
    }
}

// ---- events → actions ------------------------------------------------------------------

/// Translate one terminal event. Keys go through the keymap; the left button is
/// `Press`/`Drag`/`Release` (a click *is* the press, resolved by the loop through the last
/// `HitMap`); the wheel is `ScrollUp`/`ScrollDown(WHEEL_LINES)` — the loop routes it to the
/// nav when the pointer is over it (`pointer`); a resize is `Resize`. Focus changes, other
/// buttons, bare pointer motion and sideways scrolling are nothing.
pub fn to_action(event: &Event, keymap: &Keymap) -> Option<Action> {
    match event {
        Event::Key(k) => keymap.lookup(Key::of(k)?),
        Event::Mouse(m) => mouse_action(m),
        Event::Resize(w, h) => Some(Action::Resize(*w, *h)),
        _ => None,
    }
}

fn mouse_action(m: &MouseEvent) -> Option<Action> {
    Some(match m.kind {
        MouseEventKind::Down(MouseButton::Left) => Action::Press(m.column, m.row),
        MouseEventKind::Drag(MouseButton::Left) => Action::Drag(m.column, m.row),
        MouseEventKind::Up(_) => Action::Release,
        MouseEventKind::ScrollUp => Action::ScrollUp(WHEEL_LINES),
        MouseEventKind::ScrollDown => Action::ScrollDown(WHEEL_LINES),
        _ => return None,
    })
}

/// The pointer position of a mouse event (`None` for anything else).
pub fn pointer(event: &Event) -> Option<(u16, u16)> {
    match event {
        Event::Mouse(m) => Some((m.column, m.row)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::testfix::*;
    use crate::tui::app::{App, Changed, Effect, Focus, Selection, Target};
    use crate::tui::render::{HitMap, render};
    use lastcall_engine::engine::{AcceptRequest, RestoreRequest};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;

    fn key(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    fn key_code(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn keys(entries: &[(&str, &[&str])]) -> BTreeMap<String, KeySpecs> {
        entries
            .iter()
            .map(|(name, specs)| {
                let specs = match specs {
                    [one] => KeySpecs::One((*one).to_owned()),
                    many => KeySpecs::Many(many.iter().map(|s| (*s).to_owned()).collect()),
                };
                ((*name).to_owned(), specs)
            })
            .collect()
    }

    /// A 100×30 `TestBackend` frame of `app`: its text and the hit map it produced.
    fn frame(app: &App) -> (String, HitMap) {
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut hits = HitMap::default();
        term.draw(|f| hits = render(app, f)).unwrap();
        (term.backend().to_string(), hits)
    }

    /// Feed a key through the same path the loop uses: `to_action` then `App::handle`.
    fn press_key(app: &mut App, keymap: &Keymap, event: Event) -> (Changed, Option<Effect>) {
        let action = to_action(&event, keymap).expect("bound");
        app.handle(action)
    }

    /// Click the rendered `target` through the loop's path: a left-button `Down` becomes
    /// `Press(x, y)`, resolved against the hit map, then `App::hit`.
    fn click(
        app: &mut App,
        keymap: &Keymap,
        hits: &HitMap,
        target: &Target,
    ) -> (Changed, Option<Effect>) {
        let (rect, _) = hits
            .targets
            .iter()
            .find(|(_, t)| t == target)
            .unwrap_or_else(|| panic!("{target:?} is not on screen"));
        let event = mouse(
            MouseEventKind::Down(MouseButton::Left),
            rect.x + rect.width / 2,
            rect.y,
        );
        let Some(Action::Press(x, y)) = to_action(&event, keymap) else {
            panic!("a left press is Press");
        };
        let hit = hits
            .at(x, y)
            .expect("the press lands on the target")
            .clone();
        assert_eq!(&hit, target);
        app.hit(hit)
    }

    // ---- keymap ----------------------------------------------------------------------

    #[test]
    fn input_defaults_parse_and_every_default_key_is_bound_once() {
        let km = Keymap::defaults();
        let specs: usize = DEFAULT_KEYMAP.iter().map(|(_, s)| s.len()).sum();
        assert_eq!(km.bindings().len(), specs, "one binding per default spec");
        for (i, (k, _)) in km.bindings().iter().enumerate() {
            assert!(
                km.bindings()[..i].iter().all(|(o, _)| o != k),
                "{k:?} bound twice"
            );
        }
        assert_eq!(
            km.table(),
            App::new().keymap,
            "the table is what App seeds itself with"
        );
        assert_eq!(Keymap::default(), km);
    }

    #[test]
    fn input_every_key_action_has_a_default_binding() {
        let km = Keymap::defaults();
        for (name, _) in DEFAULT_KEYMAP {
            let action = Action::from_name(name).unwrap();
            assert!(
                km.bindings().iter().any(|(_, a)| *a == action),
                "{name} has no default key"
            );
        }
        // The wheel-only actions are bindable but unbound by default.
        assert!(
            km.bindings()
                .iter()
                .all(|(_, a)| !matches!(a, Action::ScrollUp(_) | Action::ScrollDown(_)))
        );
    }

    #[test]
    fn input_ctrl_c_and_q_both_quit_by_default() {
        let km = Keymap::defaults();
        assert_eq!(to_action(&key('q'), &km), Some(Action::Quit));
        assert_eq!(
            to_action(&key_code(KeyCode::Char('c'), KeyModifiers::CONTROL), &km),
            Some(Action::Quit)
        );
        assert_eq!(
            to_action(&key_code(KeyCode::Char('C'), KeyModifiers::CONTROL), &km),
            Some(Action::Quit),
            "a ctrl chord is case-insensitive"
        );
        assert_eq!(
            to_action(&key_code(KeyCode::Esc, KeyModifiers::NONE), &km),
            Some(Action::Back),
            "Esc never quits"
        );
    }

    #[test]
    fn input_unknown_action_is_an_error_naming_it() {
        let err = Keymap::from_config(&keys(&[("frobnicate", &["x"])])).unwrap_err();
        assert_eq!(
            err,
            KeymapError::UnknownAction {
                action: "frobnicate".into()
            }
        );
        let text = err.to_string();
        assert!(text.contains("frobnicate"), "{text}");
        assert!(text.contains("quit"), "lists the real names: {text}");
    }

    #[test]
    fn input_unparsable_spec_is_an_error() {
        for spec in ["", "ctrl-", "bogus", "f13", "ctrl-ctrl-c", "ab"] {
            let err = Keymap::from_config(&keys(&[("quit", &[spec])])).unwrap_err();
            assert!(
                matches!(&err, KeymapError::BadSpec { action, spec: s, .. } if action == "quit" && s == spec),
                "{spec:?}: {err:?}"
            );
            assert!(err.to_string().contains("quit"), "{err}");
        }
        let err = Keymap::from_config(&keys(&[("quit", &[])])).unwrap_err();
        assert!(
            matches!(&err, KeymapError::BadSpec { action, .. } if action == "quit"),
            "an empty list is refused: {err:?}"
        );
    }

    #[test]
    fn input_duplicate_binding_after_merge_is_an_error() {
        let err = Keymap::from_config(&keys(&[("refresh", &["q"])])).unwrap_err();
        assert_eq!(
            err,
            KeymapError::Duplicate {
                spec: "q".into(),
                first: "refresh".into(),
                second: "quit".into()
            }
        );
        assert!(err.to_string().contains("refresh") && err.to_string().contains("quit"));
        // Detected on the parsed key, so spelling does not hide a clash.
        assert!(matches!(
            Keymap::from_config(&keys(&[("help", &["Ctrl-C"])])),
            Err(KeymapError::Duplicate { .. })
        ));
        assert!(matches!(
            Keymap::from_config(&keys(&[("help", &["shift-tab"]), ("open", &["backtab"])])),
            Err(KeymapError::Duplicate { .. })
        ));
        // Two overrides that swap keys are fine: each replaced its defaults first.
        let km = Keymap::from_config(&keys(&[("quit", &["r"]), ("refresh", &["q"])])).unwrap();
        assert_eq!(to_action(&key('r'), &km), Some(Action::Quit));
        assert_eq!(to_action(&key('q'), &km), Some(Action::Refresh));
    }

    /// Deliverable 11: the four keys Phase 8 added are configurable like every other one —
    /// `[keys]` resolves them by name, the override replaces the default, and the help
    /// overlay has a short description for each (design review F15).
    #[test]
    fn keys_config_accepts_the_phase8_actions() {
        for (name, default, action) in [
            ("edit", 'i', Action::Edit),
            ("edit_external", 'I', Action::EditExternal),
            ("select", 'v', Action::Select),
            ("copy", 'y', Action::Copy),
        ] {
            assert_eq!(Action::from_name(name), Some(action.clone()), "{name}");
            let described = Action::describe(name);
            assert!(!described.is_empty(), "{name} has no help row");
            assert!(
                described.chars().count() <= 30,
                "{name}: {described:?} would cost the overlay its second column"
            );
            // The default, then an override of it: `ctrl-t` is bound to nothing, so the
            // only way it can resolve is through the config.
            let km = Keymap::defaults();
            assert_eq!(
                to_action(&key(default), &km),
                Some(action.clone()),
                "{name}"
            );
            let km = Keymap::from_config(&keys(&[(name, &["ctrl-t"])])).unwrap();
            assert_eq!(
                to_action(&key_code(KeyCode::Char('t'), KeyModifiers::CONTROL), &km),
                Some(action.clone()),
                "{name} rebinds"
            );
            assert_eq!(
                to_action(&key(default), &km),
                None,
                "{name}'s default is replaced, not appended"
            );
        }
        // And an unknown name is still an error, so a typo is not silently a no-op.
        assert!(matches!(
            Keymap::from_config(&keys(&[("copy_selection", &["y"])])),
            Err(KeymapError::UnknownAction { .. })
        ));
    }

    #[test]
    fn input_override_replaces_defaults_rather_than_appending() {
        // `z`, not `v`: `v` is `select`'s default since deliverable 9, and binding it to a
        // second action is the `Duplicate` this test is not about.
        let km = Keymap::from_config(&keys(&[("quit", &["x"]), ("scroll_up", &["z"])])).unwrap();
        assert_eq!(to_action(&key('x'), &km), Some(Action::Quit));
        assert_eq!(to_action(&key('q'), &km), None, "q no longer quits");
        assert_eq!(
            to_action(&key_code(KeyCode::Char('c'), KeyModifiers::CONTROL), &km),
            None,
            "ctrl-c no longer quits either: the entry replaced both defaults"
        );
        assert_eq!(to_action(&key('z'), &km), Some(Action::ScrollUp(1)));
        let table = km.table();
        let quit = table.iter().position(|(n, _)| n == "quit").unwrap();
        assert_eq!(table[quit].1, vec!["x".to_owned()]);
        assert_eq!(
            quit,
            DEFAULT_KEYMAP.len() - 1,
            "help-overlay order is preserved"
        );
        assert_eq!(
            table.last().unwrap(),
            &("scroll_up".to_owned(), vec!["z".to_owned()]),
            "a newly bound action is appended"
        );
        let untouched = table.iter().find(|(n, _)| n == "nav_up").unwrap();
        assert_eq!(untouched.1, vec!["up".to_owned(), "k".to_owned()]);
    }

    #[test]
    fn input_key_spec_grammar() {
        let k = |code, ctrl, alt, shift| Key {
            code,
            ctrl,
            alt,
            shift,
        };
        assert_eq!(
            Key::parse("q"),
            Ok(k(KeyCode::Char('q'), false, false, false))
        );
        assert_eq!(
            Key::parse("Q"),
            Ok(k(KeyCode::Char('q'), false, false, false))
        );
        assert_eq!(
            Key::parse("shift-k"),
            Ok(k(KeyCode::Char('K'), false, false, false))
        );
        assert_eq!(Key::parse("Ctrl-C"), Key::parse("ctrl-c"));
        assert_eq!(
            Key::parse("ctrl-c"),
            Ok(k(KeyCode::Char('c'), true, false, false))
        );
        assert_eq!(Key::parse("alt-up"), Ok(k(KeyCode::Up, false, true, false)));
        assert_eq!(
            Key::parse("shift-up"),
            Ok(k(KeyCode::Up, false, false, true))
        );
        assert_eq!(
            Key::parse("ctrl-alt-delete"),
            Ok(k(KeyCode::Delete, true, true, false))
        );
        assert_eq!(Key::parse("shift-tab"), Key::parse("backtab"));
        assert_eq!(
            Key::parse("backtab"),
            Ok(k(KeyCode::BackTab, false, false, false))
        );
        assert_eq!(
            Key::parse("space"),
            Ok(k(KeyCode::Char(' '), false, false, false))
        );
        assert_eq!(Key::parse("F5"), Ok(k(KeyCode::F(5), false, false, false)));
        assert_eq!(
            Key::parse("-"),
            Ok(k(KeyCode::Char('-'), false, false, false))
        );
        assert_eq!(
            Key::parse("ctrl--"),
            Ok(k(KeyCode::Char('-'), true, false, false))
        );
        assert_eq!(
            Key::parse("?"),
            Ok(k(KeyCode::Char('?'), false, false, false))
        );
        assert_eq!(
            Key::parse("]"),
            Ok(k(KeyCode::Char(']'), false, false, false))
        );
        for bad in [
            "",
            " ",
            "f0",
            "f13",
            "esc-",
            "ctrl-shift",
            "meta-x",
            "ctrl-ctrl-x",
        ] {
            assert!(Key::parse(bad).is_err(), "{bad:?} parsed");
        }
        for (name, code) in NAMED_KEYS {
            assert_eq!(Key::parse(name).unwrap().code, *code);
        }
    }

    #[test]
    fn input_key_events_normalize_like_specs() {
        let ev = |code, m| KeyEvent::new(code, m);
        // The glyph carries shift: `K` with SHIFT is `shift-k`.
        assert_eq!(
            Key::of(&ev(KeyCode::Char('K'), KeyModifiers::SHIFT)),
            Some(Key::parse("shift-k").unwrap())
        );
        assert_eq!(
            Key::of(&ev(KeyCode::Char('?'), KeyModifiers::SHIFT)),
            Some(Key::parse("?").unwrap())
        );
        assert_eq!(
            Key::of(&ev(KeyCode::Tab, KeyModifiers::SHIFT)),
            Some(Key::parse("backtab").unwrap())
        );
        assert_eq!(
            Key::of(&ev(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(Key::parse("shift-tab").unwrap())
        );
        assert_eq!(
            Key::of(&ev(
                KeyCode::Char('C'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some(Key::parse("ctrl-c").unwrap())
        );
        assert_eq!(
            Key::of(&ev(KeyCode::Up, KeyModifiers::ALT)),
            Some(Key::parse("alt-up").unwrap())
        );
        assert_eq!(
            Key::of(&KeyEvent::new_with_kind(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
                KeyEventKind::Release
            )),
            None,
            "a release is not a press"
        );
        assert_eq!(
            Key::of(&KeyEvent::new_with_kind(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
                KeyEventKind::Repeat
            )),
            Some(Key::parse("q").unwrap()),
            "a held key repeats"
        );
    }

    #[test]
    fn input_mouse_resize_and_focus_events_translate() {
        let km = Keymap::defaults();
        let left = MouseButton::Left;
        assert_eq!(
            to_action(&mouse(MouseEventKind::Down(left), 7, 3), &km),
            Some(Action::Press(7, 3))
        );
        assert_eq!(
            to_action(&mouse(MouseEventKind::Drag(left), 9, 3), &km),
            Some(Action::Drag(9, 3))
        );
        assert_eq!(
            to_action(&mouse(MouseEventKind::Up(left), 9, 3), &km),
            Some(Action::Release)
        );
        assert_eq!(
            to_action(&mouse(MouseEventKind::ScrollUp, 50, 10), &km),
            Some(Action::ScrollUp(WHEEL_LINES))
        );
        assert_eq!(
            to_action(&mouse(MouseEventKind::ScrollDown, 50, 10), &km),
            Some(Action::ScrollDown(WHEEL_LINES))
        );
        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Drag(MouseButton::Middle),
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            assert_eq!(to_action(&mouse(kind, 1, 1), &km), None, "{kind:?}");
        }
        assert_eq!(
            pointer(&mouse(MouseEventKind::ScrollUp, 50, 10)),
            Some((50, 10))
        );
        assert_eq!(pointer(&key('q')), None);
        assert_eq!(
            to_action(&Event::Resize(120, 40), &km),
            Some(Action::Resize(120, 40))
        );
        assert_eq!(to_action(&Event::FocusGained, &km), None);
        assert_eq!(to_action(&Event::FocusLost, &km), None);
        assert_eq!(to_action(&key('Z'), &km), None, "unbound keys are nothing");
    }

    // ---- mouse ⇄ keyboard parity (kickoff deliverable 9) ----------------------------

    /// `↓`×4 to beta's repo row vs a click on it.
    #[test]
    fn keymap_table_shows_the_canonical_spelling_of_an_override() {
        for (_, specs) in DEFAULT_KEYMAP {
            for spec in *specs {
                let key = Key::parse(spec).unwrap();
                // `shift-a` and `shift-u` are the defaults spelled by their modifier (a
                // bare `A` would case-fold onto `a`); the canonical form is the capital,
                // and it round-trips.
                let upper = spec.strip_prefix("shift-").map(str::to_uppercase);
                let canonical = upper.as_deref().unwrap_or(spec);
                assert_eq!(key.spec(), canonical, "default {spec} is canonical");
            }
        }
        assert!(
            App::new()
                .keymap
                .iter()
                .any(|(n, s)| n == "accept_file" && s == &["A".to_owned()]),
            "App seeds the canonical table: shift-a shows as A"
        );
        let keys: BTreeMap<String, KeySpecs> = [
            ("quit".to_owned(), KeySpecs::One("Q".to_owned())),
            (
                "hunk_next".to_owned(),
                KeySpecs::Many(vec![
                    "shift-k".to_owned(),
                    "Ctrl-N".to_owned(),
                    "Shift-Tab".to_owned(),
                ]),
            ),
        ]
        .into_iter()
        .collect();
        let km = Keymap::from_config(&keys).unwrap();
        let table = km.table();
        let row = |n: &str| table.iter().find(|(name, _)| name == n).unwrap().1.clone();
        assert_eq!(row("quit"), vec!["q"]);
        assert_eq!(row("hunk_next"), vec!["K", "ctrl-n", "backtab"]);
        assert_eq!(Action::describe("scroll_up"), "scroll the diff up");
        assert_eq!(Action::describe("scroll_down"), "scroll the diff down");
    }

    #[test]
    fn input_parity_select_repo() {
        let km = Keymap::defaults();
        let mut base = three_roots();
        base.handle(Action::Resize(100, 30));
        let (_, hits) = frame(&base);
        let mut by_key = base.clone();
        let mut by_mouse = base;

        for _ in 0..4 {
            press_key(
                &mut by_key,
                &km,
                key_code(KeyCode::Down, KeyModifiers::NONE),
            );
        }
        assert_eq!(by_key.selection, Some(Selection::Root(root("beta"))));
        click(&mut by_mouse, &km, &hits, &Target::NavRoot(root("beta")));

        assert_eq!(by_key, by_mouse);
        assert_eq!(frame(&by_key).0, frame(&by_mouse).0);
    }

    /// `↓`,`↓`,`Enter` to alpha's `f1` (the diff focused) vs a click on the row.
    #[test]
    fn input_parity_select_file() {
        let km = Keymap::defaults();
        let mut base = three_roots();
        base.handle(Action::Resize(100, 30));
        let (_, hits) = frame(&base);
        let mut by_key = base.clone();
        let mut by_mouse = base;

        press_key(
            &mut by_key,
            &km,
            key_code(KeyCode::Down, KeyModifiers::NONE),
        );
        press_key(
            &mut by_key,
            &km,
            key_code(KeyCode::Down, KeyModifiers::NONE),
        );
        press_key(
            &mut by_key,
            &km,
            key_code(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(by_key.selection, Some(row("alpha", "f1")));
        click(
            &mut by_mouse,
            &km,
            &hits,
            &Target::NavRow(root("alpha"), b"f1".to_vec()),
        );

        assert_eq!(by_key, by_mouse);
        assert_eq!(frame(&by_key).0, frame(&by_mouse).0);
    }

    /// `n` vs a click on hunk 2's header, from hunk 1 of a two-hunk row.
    #[test]
    fn input_parity_hunk_next() {
        let km = Keymap::defaults();
        let mut base = three_roots();
        base.handle(Action::Resize(100, 30));
        base.apply(pile_event("alpha", alpha_two_hunks()));
        base.select(Some(row("alpha", "f1")));
        base.handle(Action::Open);
        let (_, hits) = frame(&base);
        let mut by_key = base.clone();
        let mut by_mouse = base;

        press_key(&mut by_key, &km, key('n'));
        assert_eq!(by_key.diff.hunk, 1);
        click(&mut by_mouse, &km, &hits, &Target::DiffHunk(1));

        assert_eq!(by_key, by_mouse);
        assert_eq!(frame(&by_key).0, frame(&by_mouse).0);
    }

    /// `p` vs a click on hunk 1's header, from hunk 2 (scrolled back so both headers show).
    #[test]
    fn input_parity_hunk_prev() {
        let km = Keymap::defaults();
        let mut base = three_roots();
        base.handle(Action::Resize(100, 30));
        base.apply(pile_event("alpha", alpha_two_hunks()));
        base.select(Some(row("alpha", "f1")));
        base.handle(Action::Open);
        base.handle(Action::HunkNext);
        assert_eq!(base.diff.hunk, 1);
        base.handle(Action::ScrollUp(u16::MAX));
        assert_eq!(base.diff.scroll, 0, "hunk 1's header is on screen again");
        let (_, hits) = frame(&base);
        let mut by_key = base.clone();
        let mut by_mouse = base;

        press_key(&mut by_key, &km, key('p'));
        assert_eq!(by_key.diff.hunk, 0);
        click(&mut by_mouse, &km, &hits, &Target::DiffHunk(0));

        assert_eq!(by_key, by_mouse);
        assert_eq!(frame(&by_key).0, frame(&by_mouse).0);
    }

    /// Ruling 2: `right` is `open` and `left` is `back`, exactly as `l` and `h` are —
    /// same `App`, same frame, so the arrows are a third spelling and not a second path.
    #[test]
    fn input_parity_arrows_match_h_and_l() {
        let km = Keymap::defaults();
        let mut base = three_roots();
        base.handle(Action::Resize(100, 30));
        base.apply(pile_event("alpha", alpha_two_hunks()));
        base.select(Some(row("alpha", "f1")));
        base.handle(Action::HunkNext);

        // Right / `l` open the diff with the cursor left on hunk 2.
        let mut by_letter = base.clone();
        let mut by_arrow = base;
        press_key(&mut by_letter, &km, key('l'));
        press_key(
            &mut by_arrow,
            &km,
            key_code(KeyCode::Right, KeyModifiers::NONE),
        );
        assert_eq!(by_letter.effective_focus(), Focus::Diff);
        assert_eq!(by_letter.diff.hunk, 1, "the row's current hunk is kept");
        assert_eq!(by_letter, by_arrow);
        assert_eq!(frame(&by_letter).0, frame(&by_arrow).0);

        // Left / `h` come back to the nav with the same row selected.
        let selected = by_letter.selection.clone();
        press_key(&mut by_letter, &km, key('h'));
        press_key(
            &mut by_arrow,
            &km,
            key_code(KeyCode::Left, KeyModifiers::NONE),
        );
        assert_eq!(by_letter.effective_focus(), Focus::Nav);
        assert_eq!(by_letter.selection, selected);
        assert_eq!(by_letter, by_arrow);
        assert_eq!(frame(&by_letter).0, frame(&by_arrow).0);
    }

    /// `[keys] back = ["left"]` replaces the whole default list like any other override:
    /// `left` still means `back`, `esc` and `h` are unbound, and `open` is untouched.
    #[test]
    fn keymap_back_can_be_rebound_to_left() {
        let km = Keymap::from_config(&keys(&[("back", &["left"])])).expect("left is a key spec");
        let table = km.table();
        let row = |n: &str| table.iter().find(|(name, _)| name == n).unwrap().1.clone();
        assert_eq!(row("back"), vec!["left"]);
        assert_eq!(row("open"), vec!["enter", "l", "right"], "untouched");
        let bound = |ev: Event| to_action(&ev, &km);
        assert_eq!(
            bound(key_code(KeyCode::Left, KeyModifiers::NONE)),
            Some(Action::Back)
        );
        assert_eq!(bound(key_code(KeyCode::Esc, KeyModifiers::NONE)), None);
        assert_eq!(bound(key('h')), None);
        assert_eq!(
            bound(key_code(KeyCode::Right, KeyModifiers::NONE)),
            Some(Action::Open)
        );
    }

    #[test]
    fn input_default_keymap_has_no_duplicate_binding() {
        let mut seen: Vec<&str> = Vec::new();
        for (name, specs) in DEFAULT_KEYMAP {
            assert!(!specs.is_empty(), "{name} has no default key");
            for spec in *specs {
                assert!(!seen.contains(spec), "{spec} bound twice (second: {name})");
                seen.push(spec);
            }
        }
        let mut names: Vec<&str> = DEFAULT_KEYMAP.iter().map(|(n, _)| *n).collect();
        names.dedup();
        assert_eq!(names.len(), DEFAULT_KEYMAP.len(), "duplicate action name");
    }

    #[test]
    fn input_every_keymap_name_resolves_and_has_a_description() {
        for (name, _) in DEFAULT_KEYMAP {
            assert!(Action::from_name(name).is_some(), "{name} is not an action");
            assert!(
                !Action::describe(name).is_empty(),
                "{name} has no description"
            );
        }
        assert_eq!(Action::from_name("bogus"), None);
        assert_eq!(
            Action::from_name("scroll_down"),
            Some(Action::ScrollDown(1))
        );
    }

    /// Every `Action` variant is reachable from a default key or a mouse/terminal gesture.
    #[test]
    fn input_every_action_is_reachable() {
        let by_key: Vec<Action> = DEFAULT_KEYMAP
            .iter()
            .filter_map(|(n, _)| Action::from_name(n))
            .collect();
        let table: &[(Action, &str)] = &[
            (Action::NavUp, "key"),
            (Action::NavDown, "key"),
            (Action::NavPageUp, "key"),
            (Action::NavPageDown, "key"),
            (Action::Open, "key"),
            (Action::Back, "key"),
            (Action::FocusToggle, "key"),
            (Action::HunkNext, "key"),
            (Action::HunkPrev, "key"),
            (Action::Expand, "key"),
            (Action::ScrollUp(3), "wheel"),
            (Action::ScrollDown(3), "wheel"),
            (Action::ToggleFullPaths, "key"),
            (Action::ToggleRemote, "key"),
            (Action::HideEmpty, "key"),
            (Action::Refresh, "key"),
            (Action::Help, "key"),
            (Action::Quit, "key"),
            (Action::Accept, "key"),
            (Action::AcceptFile, "key"),
            (Action::AcceptAll, "key"),
            (Action::Restore, "key"),
            (Action::RestoreFile, "key"),
            (Action::Flag, "key"),
            (Action::Unflag, "key"),
            (Action::Select, "key"),
            (Action::Copy, "key"),
            (Action::SelectTo(0), "mouse"),
            (Action::Edit, "key"),
            (Action::EditExternal, "key"),
            (Action::Note(NoteKey::Send), "modal-note"),
            (Action::Editor(EditorKey::Save), "modal-note"),
            (Action::Pick(PickKey::Send), "modal-note"),
            (Action::Ack, "key"),
            (Action::Jump, "key"),
            (Action::ScopeToggle, "key"),
            (Action::Confirm, "modal"),
            (Action::Cancel, "modal"),
            (Action::Press(1, 1), "mouse"),
            (Action::Drag(1, 1), "mouse"),
            (Action::Release, "mouse"),
            (Action::Resize(80, 24), "terminal"),
            (Action::Tick, "timer"),
            (Action::Herdr(HerdrUpdate::Reconnecting), "herdr"),
        ];
        let by_modal: Vec<Action> = MODAL_KEYS
            .iter()
            .flat_map(|(_, specs)| specs.iter())
            .filter_map(|s| modal_action(Key::parse(s).unwrap()))
            .collect();
        for (action, source) in table {
            match *source {
                "key" => assert!(by_key.contains(action), "{action:?} has no default key"),
                "modal" => {
                    assert!(by_modal.contains(action), "{action:?} has no modal key");
                    assert!(!by_key.contains(action), "{action:?} must not be key-bound");
                }
                _ => assert!(!by_key.contains(action), "{action:?} must not be key-bound"),
            }
        }
        // The table is the exhaustive variant list: a new variant must be added here.
        let _exhaustive = |a: Action| match a {
            Action::NavUp
            | Action::NavDown
            | Action::NavPageUp
            | Action::NavPageDown
            | Action::Open
            | Action::Back
            | Action::FocusToggle
            | Action::HunkNext
            | Action::HunkPrev
            | Action::Expand
            | Action::ScrollUp(_)
            | Action::ScrollDown(_)
            | Action::ToggleFullPaths
            | Action::ToggleRemote
            | Action::HideEmpty
            | Action::Refresh
            | Action::Help
            | Action::Quit
            | Action::Press(_, _)
            | Action::Drag(_, _)
            | Action::Release
            | Action::Resize(_, _)
            | Action::Tick
            | Action::Accept
            | Action::AcceptFile
            | Action::AcceptAll
            | Action::Restore
            | Action::RestoreFile
            | Action::Flag
            | Action::Unflag
            | Action::Select
            | Action::Copy
            | Action::SelectTo(_)
            | Action::Edit
            | Action::EditExternal
            | Action::Note(_)
            | Action::Editor(_)
            | Action::Pick(_)
            | Action::Confirm
            | Action::Cancel
            | Action::Ack
            | Action::Jump
            | Action::ScopeToggle
            | Action::Herdr(_) => 44,
        };
        assert_eq!(table.len(), 44);
    }

    #[test]
    fn input_modal_keys_resolve_only_through_modal_action() {
        let km = Keymap::defaults();
        let y = Key::parse("y").unwrap();
        let enter = Key::parse("enter").unwrap();
        let n = Key::parse("n").unwrap();
        let esc = Key::parse("esc").unwrap();
        assert_eq!(modal_action(y), Some(Action::Confirm));
        assert_eq!(modal_action(enter), Some(Action::Confirm));
        assert_eq!(modal_action(n), Some(Action::Cancel));
        assert_eq!(modal_action(esc), Some(Action::Cancel));
        assert_eq!(modal_action(Key::parse("a").unwrap()), None);
        // Outside the modal the same keys keep their keymap meaning (or none).
        assert_eq!(to_action(&key('n'), &km), Some(Action::HunkNext));
        // `y` is the confirm key *inside* the modal and the copy key outside it
        // (deliverable 9): the two never collide, because `modal_action` is consulted only
        // while `App::confirm` is open and the keymap only when it is not.
        assert_eq!(to_action(&key('y'), &km), Some(Action::Copy));
        assert!(
            km.bindings()
                .iter()
                .all(|(_, a)| !matches!(a, Action::Confirm | Action::Cancel)),
            "confirm/cancel are never in the keymap"
        );
        // …and `[keys]` cannot bind them in v1.
        for name in ["confirm", "cancel"] {
            assert!(matches!(
                Keymap::from_config(&keys(&[(name, &["x"])])),
                Err(KeymapError::UnknownAction { .. })
            ));
        }
    }

    #[test]
    fn input_accept_keys_by_default() {
        let km = Keymap::defaults();
        assert_eq!(to_action(&key('a'), &km), Some(Action::Accept));
        assert_eq!(
            to_action(&key_code(KeyCode::Char('A'), KeyModifiers::SHIFT), &km),
            Some(Action::AcceptFile)
        );
        assert_eq!(
            to_action(&key_code(KeyCode::Char('a'), KeyModifiers::CONTROL), &km),
            Some(Action::AcceptAll)
        );
        // `[keys]` overrides cover the three accept actions.
        let km = Keymap::from_config(&keys(&[("accept", &["x"]), ("accept_all", &["shift-x"])]))
            .unwrap();
        assert_eq!(to_action(&key('x'), &km), Some(Action::Accept));
        assert_eq!(to_action(&key('a'), &km), None);
        assert_eq!(to_action(&key('X'), &km), Some(Action::AcceptAll));
    }

    // ---- accept parity (kickoff deliverable 7) ----------------------------------------

    /// `n`, `n`, `a` vs a click on hunk 2's `[a accept]`: equal apps, equal effects, and
    /// the effect is one `AcceptRequest::Hunk` for index 2 of 3.
    #[test]
    fn input_parity_accept_hunk() {
        let km = Keymap::defaults();
        let mut base = three_roots();
        base.handle(Action::Resize(100, 30));
        base.apply(pile_event("alpha", alpha_hunks(3)));
        base.select(Some(row("alpha", "f1")));
        base.handle(Action::Open);
        let (_, hits) = frame(&base);
        let mut by_key = base.clone();
        let mut by_mouse = base;

        press_key(&mut by_key, &km, key('n'));
        press_key(&mut by_key, &km, key('n'));
        assert_eq!(by_key.diff.hunk, 2);
        let by_key_effect = press_key(&mut by_key, &km, key('a'));
        let by_mouse_effect = click(&mut by_mouse, &km, &hits, &Target::HunkAccept(2));

        assert_eq!(by_key, by_mouse);
        assert_eq!(by_key_effect, by_mouse_effect);
        assert_eq!(frame(&by_key).0, frame(&by_mouse).0);
        let Some(Effect::Accept(reqs)) = by_key_effect.1 else {
            panic!("an accept effect: {by_key_effect:?}");
        };
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].0, root("alpha"));
        match &reqs[0].1 {
            AcceptRequest::Hunk { index, hunks, .. } => {
                assert_eq!((*index, hunks.len()), (2, 3));
            }
            other => panic!("a hunk request: {other:?}"),
        }
        assert!(by_key.accepting.is_some());
    }

    /// `A` vs a click on the main-view header's `[A accept file]`, from the nav pane.
    #[test]
    fn input_parity_accept_file() {
        let km = Keymap::defaults();
        let mut base = three_roots();
        base.handle(Action::Resize(100, 30));
        base.select(Some(row("alpha", "f1")));
        let (_, hits) = frame(&base);
        let mut by_key = base.clone();
        let mut by_mouse = base;

        let by_key_effect = press_key(
            &mut by_key,
            &km,
            key_code(KeyCode::Char('A'), KeyModifiers::SHIFT),
        );
        let by_mouse_effect = click(&mut by_mouse, &km, &hits, &Target::FileAccept);

        assert_eq!(by_key, by_mouse);
        assert_eq!(by_key_effect, by_mouse_effect);
        assert_eq!(frame(&by_key).0, frame(&by_mouse).0);
        let Some(Effect::Accept(reqs)) = by_key_effect.1 else {
            panic!("an accept effect: {by_key_effect:?}");
        };
        assert_eq!(reqs.len(), 1);
        assert!(
            matches!(reqs[0].1, AcceptRequest::File(_)),
            "{:?}",
            reqs[0].1
        );
    }

    /// `u` on hunk 2 vs a click on that hunk's `[u restore]`, and `U` vs a click on the
    /// header's `[U restore file]`. Both halves land on one `App`, one `Effect` and one
    /// frame — the working-tree write has the same key/mouse parity as the accept beside it.
    #[test]
    fn input_parity_restore() {
        let km = Keymap::defaults();
        let mut base = three_roots();
        base.handle(Action::Resize(100, 30));
        base.apply(pile_event("alpha", alpha_hunks(3)));
        base.select(Some(row("alpha", "f1")));
        base.handle(Action::Open);
        let (_, hits) = frame(&base);
        let mut by_key = base.clone();
        let mut by_mouse = base.clone();

        press_key(&mut by_key, &km, key('n'));
        press_key(&mut by_key, &km, key('n'));
        assert_eq!(by_key.diff.hunk, 2);
        let by_key_effect = press_key(&mut by_key, &km, key('u'));
        let by_mouse_effect = click(&mut by_mouse, &km, &hits, &Target::HunkRestore(2));

        assert_eq!(by_key, by_mouse);
        assert_eq!(by_key_effect, by_mouse_effect);
        assert_eq!(frame(&by_key).0, frame(&by_mouse).0);
        let Some(Effect::Restore(reqs)) = by_key_effect.1 else {
            panic!("a restore effect: {by_key_effect:?}");
        };
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].0, root("alpha"));
        match &reqs[0].1 {
            RestoreRequest::Hunk { index, hunks, .. } => {
                assert_eq!((*index, hunks.len()), (2, 3));
            }
            other => panic!("a hunk request: {other:?}"),
        }
        assert!(by_key.restoring.is_some(), "the restore is in flight");
        assert!(by_key.confirm.is_none(), "a hunk restore never asks");

        // The file half: `U` and the header control both open the confirm, so neither
        // starts a restore on its own.
        let mut by_key = base.clone();
        let mut by_mouse = base;
        let by_key_effect = press_key(
            &mut by_key,
            &km,
            key_code(KeyCode::Char('U'), KeyModifiers::SHIFT),
        );
        let by_mouse_effect = click(&mut by_mouse, &km, &hits, &Target::FileRestore);
        assert_eq!(by_key, by_mouse);
        assert_eq!(by_key_effect, by_mouse_effect);
        assert_eq!(frame(&by_key).0, frame(&by_mouse).0);
        assert_eq!(by_key_effect.1, None, "the modal asks first");
        assert!(by_key.confirm.is_some(), "a file restore asks");
        let (_, effect) = by_key.handle(Action::Confirm);
        let Some(Effect::Restore(reqs)) = effect else {
            panic!("y starts the restore: {effect:?}");
        };
        assert!(
            matches!(reqs[0].1, RestoreRequest::File(_)),
            "{:?}",
            reqs[0].1
        );
    }

    /// `m` on hunk 2 vs a click on that hunk's `[m flag]`. Both open the note modal on the
    /// same target, and neither writes anything until Enter — the flag's key/mouse parity.
    #[test]
    fn input_parity_flag() {
        let km = Keymap::defaults();
        let mut base = three_roots();
        base.handle(Action::Resize(100, 30));
        base.apply(pile_event("alpha", alpha_hunks(3)));
        base.select(Some(row("alpha", "f1")));
        base.handle(Action::Open);
        let (_, hits) = frame(&base);
        let mut by_key = base.clone();
        let mut by_mouse = base;

        press_key(&mut by_key, &km, key('n'));
        press_key(&mut by_key, &km, key('n'));
        assert_eq!(by_key.diff.hunk, 2);
        let by_key_effect = press_key(&mut by_key, &km, key('m'));
        let by_mouse_effect = click(&mut by_mouse, &km, &hits, &Target::HunkFlag(2));

        assert_eq!(by_key, by_mouse);
        assert_eq!(by_key_effect, by_mouse_effect);
        assert_eq!(frame(&by_key).0, frame(&by_mouse).0);
        assert_eq!(by_key_effect.1, None, "the modal collects the note first");
        let note = by_key.note.as_ref().expect("the note modal is open");
        assert_eq!(note.target.status_label(), "f1 hunk 3");
        assert!(note.text().is_empty());
    }

    /// `e` vs a click on the `[e expand]` control of a collapsed row: both ask the engine
    /// for the same row and land on one `App` (§6.7 parity). On a **binary** row the
    /// control is not drawn at all, so there is nothing to click and the key is a no-op —
    /// the two paths agree there too.
    #[test]
    fn input_parity_expand() {
        let km = Keymap::defaults();
        let mut base = three_roots();
        base.handle(Action::Resize(100, 30));
        base.apply(pile_event_seq(
            "alpha",
            1,
            alpha_collapsed(lastcall_engine::scan::Collapsed::Glob),
        ));
        base.select(Some(row("alpha", "f1")));
        let (_, hits) = frame(&base);
        let mut by_key = base.clone();
        let mut by_mouse = base;

        let by_key_effect = press_key(&mut by_key, &km, key('e'));
        let by_mouse_effect = click(&mut by_mouse, &km, &hits, &Target::Expand);

        assert_eq!(by_key, by_mouse);
        assert_eq!(by_key_effect, by_mouse_effect);
        assert_eq!(frame(&by_key).0, frame(&by_mouse).0);
        let Some(Effect::Expand(root_asked, row_asked)) = by_key_effect.1 else {
            panic!("an expand effect: {by_key_effect:?}");
        };
        assert_eq!(root_asked, root("alpha"));
        assert_eq!(row_asked.path, b"f1");

        // Binary: no control on screen, and the key does nothing.
        let mut binary = three_roots();
        binary.handle(Action::Resize(100, 30));
        binary.apply(pile_event_seq(
            "alpha",
            1,
            alpha_collapsed(lastcall_engine::scan::Collapsed::Binary),
        ));
        binary.select(Some(row("alpha", "f1")));
        let (_, binary_hits) = frame(&binary);
        assert!(
            !binary_hits
                .targets
                .iter()
                .any(|(_, t)| *t == Target::Expand),
            "a binary row offers no expand control"
        );
        let before = binary.clone();
        assert_eq!(press_key(&mut binary, &km, key('e')), (Changed::No, None));
        assert_eq!(binary, before);
    }

    /// `d` vs a click on the nav dot: both select the root and ack it, and land on one
    /// `App`. Ruling 10's ack is local, so the only effect either produces is the toast
    /// withdrawal.
    #[test]
    fn input_parity_ack() {
        let km = Keymap::defaults();
        let mut base = three_roots();
        base.handle(Action::Resize(100, 30));
        // alpha has nothing pending; herdr says its agent is done, so it is listed anyway.
        base.apply(pile_event_seq(
            "alpha",
            1,
            without(pile("alpha"), &["f1", "f2"]),
        ));
        base.handle(Action::Herdr(HerdrUpdate::Connected {
            version: "0.8.2".to_owned(),
            protocol: 21,
        }));
        base.handle(Action::Herdr(HerdrUpdate::Roots(BTreeMap::from([(
            root("alpha"),
            crate::tui::herdr::testfix::agents(
                crate::tui::herdr::Attention::Done,
                1,
                "w1:p1",
                "claude",
            ),
        )]))));
        let (_, hits) = frame(&base);
        let mut by_key = base.clone();
        let mut by_mouse = base.clone();

        // The key path selects the root first, exactly as the click does.
        by_key.select(Some(Selection::Root(root("alpha"))));
        let by_key_effect = press_key(&mut by_key, &km, key('d'));
        let by_mouse_effect = click(&mut by_mouse, &km, &hits, &Target::RootDot(root("alpha")));

        assert_eq!(by_key, by_mouse);
        assert_eq!(by_key_effect, by_mouse_effect);
        assert_eq!(frame(&by_key).0, frame(&by_mouse).0);
        assert_eq!(
            by_key.herdr.dot(&root("alpha")),
            Some(crate::tui::herdr::Dot::Ready { acked: true }),
            "both paths dim the flag"
        );
    }

    /// `ctrl-a` vs a click on the header's `[Accept All]`: one `AcceptRequest::All` per
    /// listed root, each carrying that root's held pile; five files, so no modal.
    #[test]
    fn input_parity_accept_all() {
        let km = Keymap::defaults();
        let mut base = three_roots();
        base.handle(Action::Resize(100, 30));
        let (_, hits) = frame(&base);
        let mut by_key = base.clone();
        let mut by_mouse = base.clone();

        let by_key_effect = press_key(
            &mut by_key,
            &km,
            key_code(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        let by_mouse_effect = click(&mut by_mouse, &km, &hits, &Target::HeaderAcceptAll);

        assert_eq!(by_key, by_mouse);
        assert_eq!(by_key_effect, by_mouse_effect);
        assert_eq!(frame(&by_key).0, frame(&by_mouse).0);
        assert!(by_key.confirm.is_none(), "five files ask nothing");
        let Some(Effect::Accept(reqs)) = by_key_effect.1 else {
            panic!("an accept effect: {by_key_effect:?}");
        };
        let expected: Vec<(std::path::PathBuf, AcceptRequest)> = base
            .listed_roots()
            .map(|v| (v.meta.path.clone(), AcceptRequest::All(v.pile.clone())))
            .collect();
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs, expected, "exactly the held piles, in nav order");
    }
}
