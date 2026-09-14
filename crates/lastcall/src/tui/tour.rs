//! The first-launch welcome overlay (Amendment v1.11, deliverable 1).
//!
//! The first `lastcall tui` ever run against a state directory opens an overlay over the
//! live screen, once the launch hold and the scope verdict are past: a card of keys, then up
//! to two cards that appear only when their condition holds, each offering to change one
//! default and remember it. It is shown once, and `lastcall tui --tour` shows it again.
//!
//! What lives here: the marker on disk, the [`Plan`] the loop drives it with, the cards and
//! their wording, the fixed keys ([`tour_action`]), and the card's lines as text. The
//! painting itself is `render::render_tour`, beside every other modal's, because a hit map
//! is a fact about a frame.
//!
//! Three rules keep it from being in the way:
//!
//! * **Shown once, for good.** Any dismissal writes the marker: finishing, skipping, or
//!   quitting with it open.
//! * **Never on a screen too small to read it.** Under [`MIN_COLS`] columns or [`MIN_ROWS`]
//!   rows it does not open and writes nothing, so it is still there on the first launch that
//!   is large enough.
//! * **Nothing is written without a keystroke asking for it.** The second row of a choice
//!   card is the only thing in lastcall that writes to the config file (`config::write`),
//!   and the first row (the selected one) writes nothing at all.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthStr;

use lastcall_engine::config::write::{Document, Setting};
use lastcall_engine::env::Env;
use lastcall_engine::ledger::iso8601_date;

use super::app::{App, Changed};
use super::herdr::Link;
use super::input::{Action, Key, Keymap, TourKey, quit_only};
use super::render::key_label;

use crossterm::event::{Event, KeyCode};

/// The marker file under the state directory. Its presence (parsable or not is a separate
/// question, see [`shown_before`]) is the whole "has this been seen?" record.
pub const MARKER_FILE: &str = "first-launch.json";

/// The narrowest frame the overlay opens on. Under it the tour does not open and **nothing
/// is written**, so it is still waiting on the first launch that has room for it (F12).
pub const MIN_COLS: u16 = 60;
/// The shortest frame the overlay opens on, on the same terms.
pub const MIN_ROWS: u16 = 14;

/// The version this binary is, recorded in the marker so a later lastcall can tell which
/// build showed the welcome.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `<state_dir>/first-launch.json`. Additive to a state directory, like the update stamp:
/// nothing else reads it and no headless command writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// Unix seconds, from the engine's injected clock.
    pub shown_at: u64,
    /// The binary that showed it.
    pub version: String,
}

pub fn marker_path(state_dir: &Path) -> PathBuf {
    state_dir.join(MARKER_FILE)
}

/// Whether this state directory has already seen the welcome.
///
/// Absent, unreadable, or unparsable all mean **no**: the file is a record we wrote, and a
/// record we cannot read is not one worth refusing to help a new user over. One `stat` (and
/// at most one small read) per launch, and none at all after the overlay has been dismissed.
pub fn shown_before(state_dir: &Path) -> bool {
    std::fs::read_to_string(marker_path(state_dir))
        .ok()
        .and_then(|text| serde_json::from_str::<Marker>(&text).ok())
        .is_some()
}

/// Write the marker, `write_stamp`'s idiom: temp beside the target, then rename.
pub fn write_marker(state_dir: &Path, now: SystemTime, version: &str) -> io::Result<()> {
    let marker = Marker {
        shown_at: now
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        version: version.to_owned(),
    };
    std::fs::create_dir_all(state_dir)?;
    let path = marker_path(state_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&marker).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)
}

/// One card of the tour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Card {
    /// Always first, and always shown: what the keys are.
    Keys,
    /// Shown when lastcall is actually following a herdr workspace right now and the config
    /// file has never said whether that is wanted (F5).
    Herdr { version: String },
    /// Shown when at least [`EMPTY_CARD_MIN`] listed repositories have nothing pending, `t`
    /// is off, and the config file has never said whether that is wanted.
    Empty { empty: usize, total: usize },
}

/// How many empty repositories it takes before the tour offers to hide them. Below this the
/// list is still readable and the offer would be noise.
pub const EMPTY_CARD_MIN: usize = 10;

impl Card {
    /// The setting the second row of this card writes, or `None` for a card with no choice.
    pub fn setting(&self) -> Option<Setting> {
        match self {
            Card::Keys => None,
            Card::Herdr { .. } => Some(Setting::HerdrScopeAll),
            Card::Empty { .. } => Some(Setting::HideEmptyRepos),
        }
    }

    /// Whether this card asks a question (two rows) rather than just saying something.
    pub fn is_choice(&self) -> bool {
        self.setting().is_some()
    }
}

/// The overlay's own state while it is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tour {
    /// The cards to show, in order, decided once when the overlay opens.
    pub cards: Vec<Card>,
    /// Which card is on screen.
    pub at: usize,
    /// Which row of a choice card is selected: `0` keeps the default, `1` changes it.
    pub row: usize,
    /// A write that failed: the sentence the footer shows instead of the keys, and the TOML
    /// the user would have to add themselves. `enter` then advances (the live setting is
    /// applied for the session either way).
    pub failed: Option<String>,
}

impl Tour {
    pub fn new(cards: Vec<Card>) -> Tour {
        Tour {
            cards,
            at: 0,
            row: 0,
            failed: None,
        }
    }

    pub fn card(&self) -> &Card {
        self.cards.get(self.at).unwrap_or(&Card::Keys)
    }

    /// How many rows the card on screen offers to the arrow keys.
    pub fn rows(&self) -> usize {
        if self.card().is_choice() { 2 } else { 0 }
    }
}

/// The tour's fixed keys, resolved before the keymap and before every modal (F7).
///
/// `enter` applies the selected row or advances; the arrows and `j`/`k` move between the two
/// rows of a choice card; `q` and `esc` skip the rest. `q` here **never quits**. That is the
/// note modal's rule, because a card with a highlighted row is a question and the reader has to
/// be able to answer it without leaving. The keymap's quit action survives only in its
/// non-printable spellings (`ctrl-c` by default), and that quit writes the marker on its way
/// out. Every other key is ignored.
pub fn tour_action(event: &Event, keymap: &Keymap) -> Option<Action> {
    let Event::Key(k) = event else {
        return None;
    };
    let key = Key::of(k)?;
    let printable = !key.ctrl && !key.alt;
    let tour = match key.code {
        KeyCode::Enter => TourKey::Next,
        KeyCode::Up => TourKey::Up,
        KeyCode::Down => TourKey::Down,
        KeyCode::Char('k') if printable => TourKey::Up,
        KeyCode::Char('j') if printable => TourKey::Down,
        KeyCode::Esc => TourKey::Skip,
        // `q` is the footer's promise, whatever the keymap says it does elsewhere; a quit
        // bound to some other printable key skips too, because nothing printable may quit
        // out from under an unanswered question.
        KeyCode::Char('q') if printable => TourKey::Skip,
        _ => {
            return match quit_only(keymap, key) {
                Some(quit) => Some(quit),
                None => match keymap.lookup(key) {
                    Some(Action::Quit) => Some(Action::Tour(TourKey::Skip)),
                    _ => None,
                },
            };
        }
    };
    Some(Action::Tour(tour))
}

// ---- the loop's half ------------------------------------------------------------------

/// Everything the event loop needs to run the tour: whether to show it at all, the
/// environment the config write resolves through, and where the marker goes.
///
/// The reducer does no I/O, so every file this feature touches is touched from here: the
/// marker read at launch, the config document parsed when the overlay opens, the one config
/// write a choice asks for, and the marker written when it closes.
#[derive(Debug)]
pub struct Plan {
    /// Whether the overlay is still owed. Cleared the moment it is dismissed.
    show: bool,
    env: Env,
    state_dir: PathBuf,
    version: String,
    /// The config document, parsed once when the overlay opens.
    doc: Option<Document>,
}

impl Plan {
    /// Decide, from the marker alone, whether this launch shows the welcome. `force` is
    /// `--tour`, which ignores the marker for this run and rewrites it on dismissal.
    pub fn new(force: bool, env: Env, state_dir: PathBuf) -> Plan {
        Plan {
            show: force || !shown_before(&state_dir),
            env,
            state_dir,
            version: VERSION.to_owned(),
            doc: None,
        }
    }

    /// A plan that shows nothing, for the loop's tests.
    pub fn off(env: Env, state_dir: PathBuf) -> Plan {
        Plan {
            show: false,
            env,
            state_dir,
            version: VERSION.to_owned(),
            doc: None,
        }
    }

    /// Whether the overlay should open on the frame about to be drawn: the launch hold and
    /// the scope verdict are past (the instant `is_listed` first admits a root), the frame
    /// is big enough to read, and nothing has dismissed it yet.
    pub fn due(&self, app: &App) -> bool {
        self.show
            && app.tour.is_none()
            && app.loading.is_none()
            && !app.herdr.scope_pending
            && app.size.0 >= MIN_COLS
            && app.size.1 >= MIN_ROWS
    }

    /// Open it: parse the config document once, decide which cards apply, and hand them to
    /// the reducer.
    pub fn open(&mut self, app: &mut App) -> Changed {
        let doc = self.doc.get_or_insert_with(|| Document::open(&self.env));
        let mut cards = vec![Card::Keys];
        // F5: lastcall is following a workspace *right now* (a live link, a scope it
        // derived, and the scope honoured), and the file has never said whether that is
        // wanted. Any one of those missing and the card would be about nothing.
        if let Link::Connected { version } = &app.herdr.link
            && app.herdr.scoped
            && app.herdr.scope.is_some()
            && !doc.sets(Setting::HerdrScopeAll)
        {
            cards.push(Card::Herdr {
                version: version.clone(),
            });
        }
        let total = app.listed_roots().count();
        let empty = app.listed_roots().filter(|v| !v.listed()).count();
        if empty >= EMPTY_CARD_MIN && !app.hide_empty && !doc.sets(Setting::HideEmptyRepos) {
            cards.push(Card::Empty { empty, total });
        }
        app.tour = Some(Tour::new(cards));
        Changed::Yes
    }

    /// Perform the one config write a choice asked for. `now` is the engine's own clock, so
    /// the created file's comment carries a date this process never read off the wall.
    pub fn write(&mut self, setting: Setting, now: SystemTime) -> Result<(), String> {
        let doc = self.doc.get_or_insert_with(|| Document::open(&self.env));
        let today = iso8601_date(&lastcall_engine::ledger::iso8601(now)).to_owned();
        let path = doc
            .path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        doc.write(setting, &today)
            .map_err(|reason| failure_line(&path, &reason))
    }

    /// The overlay closed, by any path. Writes the marker once and never shows again.
    pub fn dismissed(&mut self, now: SystemTime) {
        if !self.show {
            return;
        }
        self.show = false;
        if let Err(e) = write_marker(&self.state_dir, now, &self.version) {
            // Nothing the reader can do about it, and nothing worth a modal: the cost is
            // that the welcome opens once more.
            tracing::warn!(error = %e, "could not write the first-launch marker");
        }
    }

    /// Whether the overlay is still owed. The loop asks after the event loop ends, so a
    /// quit with the tour open still writes the marker.
    pub fn owed(&self) -> bool {
        self.show
    }
}

/// The footer a failed write shows: what went wrong and where, before the line to add.
pub fn failure_line(path: &str, reason: &str) -> String {
    if path.is_empty() {
        format!("could not write the config file: {reason}. Add this line yourself:")
    } else {
        format!("could not write {path}: {reason}. Add this line yourself:")
    }
}

// ---- the card as text -----------------------------------------------------------------

/// What a line of a card is, so the painter knows how to style it and what to make clickable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Title,
    Body,
    /// Choice row `n`: a `Target::TourRow(n)` and, when selected, bold.
    Choice(usize),
    /// The keys at the bottom, or a failed write's sentence. `Target::TourRow(0)` on a card
    /// with no choice rows, so the mouse can advance a plain card.
    Footer,
}

/// One line of the card on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardLine {
    pub text: String,
    pub kind: Kind,
}

fn line(text: impl Into<String>, kind: Kind) -> CardLine {
    CardLine {
        text: text.into(),
        kind,
    }
}

/// The keys card's intro.
pub const KEYS_INTRO: &str = "The list on the left is every repository under this directory. Pick a file and the diff opens on the right.";
/// The keys card's second row, which is prose rather than a key: it spans the two right
/// columns of the grid.
pub const KEYS_PANES: &str = "tab, or left and right, move between the two panes";
/// A plain card's footer.
pub const KEYS_FOOTER: &str = "enter  next          q  skip the rest";
/// A choice card's footer.
pub const CHOICE_FOOTER: &str = "enter  choose          q  skip the rest";

/// The keys card's grid, by column: `(action name, what it does)`. The key column is
/// rendered from the **effective** keymap (F15), so a user with a `[keys]` table sees their
/// own bindings on their first launch.
const KEY_CELLS: [[(&str, &str); 3]; 4] = [
    [
        ("accept", "accept the hunk under the cursor"),
        ("accept_file", "accept the whole file"),
        ("accept_all", "accept everything"),
    ],
    [
        ("hunk_next|hunk_prev", "next and previous hunk"),
        ("", KEYS_PANES),
        ("", ""),
    ],
    [
        ("restore", "put a hunk back the way it was"),
        ("flag", "flag it with a note"),
        ("undo", "undo the last accept"),
    ],
    [
        ("hide_empty", "hide repositories with nothing pending"),
        ("snooze", "snooze a repository"),
        ("help", "every key, any time"),
    ],
];

/// The **first** key bound to `action`, labelled as the help overlay labels it. The first
/// one, not all of them: this card is a first impression, and `n / ]` in place of `n` is a
/// second thing to learn at the worst moment for it.
fn primary(app: &App, action: &str) -> String {
    match action.split_once('|') {
        Some((a, b)) => format!("{} / {}", primary(app, a), primary(app, b)),
        None => app
            .keys_for(action)
            .first()
            .map(|s| key_label(s))
            .unwrap_or_default(),
    }
}

/// One cell of the keys grid: `<key>  <what it does>`, or the phrase alone when the action
/// is unbound or the cell is prose.
fn cell(app: &App, (action, phrase): (&str, &str)) -> String {
    let keys = primary(app, action);
    if keys.is_empty() {
        phrase.to_owned()
    } else {
        format!("{keys}  {phrase}")
    }
}

impl Tour {
    /// The width this card would like for its text: its widest line, unwrapped.
    pub fn natural(&self, app: &App) -> usize {
        self.lines(app, usize::MAX, usize::MAX)
            .iter()
            .map(|l| l.text.width())
            .max()
            .unwrap_or(0)
    }

    /// The card on screen as lines, wrapped to `width` text columns and trimmed to fit
    /// `height` rows of text.
    ///
    /// `height` only ever bites on the keys card in a small frame: the grid falls back to
    /// one key per line when three columns do not fit, and eleven keys plus a title and a
    /// footer do not fit the shortest frame the overlay opens on. It gives up the intro
    /// first and then the keys themselves, from the end but never the last one: `?` is the
    /// way to all of them and is the last thing to go.
    pub fn lines(&self, app: &App, width: usize, height: usize) -> Vec<CardLine> {
        let mut out = Vec::new();
        match self.card() {
            Card::Keys => {
                let intro = wrap(KEYS_INTRO, width);
                let mut grid = keys_grid(app, width);
                // The title, the intro, the grid and the footer; the blank rows between
                // them are what the painter drops first, so they are not counted here.
                let mut solid = 1 + intro.len() + grid.len() + 1;
                let keep_intro = solid <= height;
                if !keep_intro {
                    solid -= intro.len();
                }
                while solid > height && grid.len() > 1 {
                    grid.remove(grid.len() - 2);
                    solid -= 1;
                }
                out.push(line("Welcome to lastcall", Kind::Title));
                if keep_intro {
                    out.push(line("", Kind::Body));
                    out.extend(intro.into_iter().map(|t| line(t, Kind::Body)));
                }
                out.push(line("", Kind::Body));
                out.extend(grid.into_iter().map(|t| line(t, Kind::Body)));
            }
            Card::Herdr { version } => {
                out.push(line(
                    format!("You are running inside herdr {version}. Nice!"),
                    Kind::Title,
                ));
                out.push(line("", Kind::Body));
                out.extend(
                    wrap(
                        "lastcall follows this workspace. When a pane changes directory, the list narrows to the repositories that workspace is working in, and the bottom line says how many are out of view. w shows everything for the session.",
                        width,
                    )
                    .into_iter()
                    .map(|t| line(t, Kind::Body)),
                );
                out.push(line("", Kind::Body));
                out.extend(self.choice(0, "Keep following the workspace", "", width));
                out.extend(self.choice(
                    1,
                    "Show every repository instead, and remember that",
                    "(writes scope = \"all\" under [herdr])",
                    width,
                ));
            }
            Card::Empty { empty, total } => {
                out.push(line(
                    format!("{empty} of your {total} repositories have nothing pending"),
                    Kind::Title,
                ));
                out.push(line("", Kind::Body));
                out.extend(
                    wrap(
                        "They are listed anyway so the picture is complete. t hides them for the session; the count on the bottom line keeps saying how many are hidden.",
                        width,
                    )
                    .into_iter()
                    .map(|t| line(t, Kind::Body)),
                );
                out.push(line("", Kind::Body));
                out.extend(self.choice(0, "Keep listing every repository", "", width));
                out.extend(self.choice(
                    1,
                    "Start with the empty ones hidden, and remember that",
                    "(writes hide_empty_repos = true)",
                    width,
                ));
            }
        }
        out.push(line("", Kind::Body));
        match &self.failed {
            // A write that failed replaces the keys with what went wrong and the line to
            // add by hand. The keys still work; `enter` advances, as the sentence implies.
            Some(message) => {
                out.extend(
                    wrap(message, width)
                        .into_iter()
                        .map(|t| line(t, Kind::Footer)),
                );
                for toml in self.card().setting().map(Setting::lines).unwrap_or(&[]) {
                    out.push(line(format!("    {toml}"), Kind::Footer));
                }
            }
            None if self.card().is_choice() => out.push(line(CHOICE_FOOTER, Kind::Footer)),
            None => out.push(line(KEYS_FOOTER, Kind::Footer)),
        }
        out
    }

    /// One choice row: the marker, the sentence, and what it writes. Narrow terminals put
    /// the parenthetical on its own indented line rather than letting it fall off the edge;
    /// both lines are the same `Choice(n)`, so either one is clickable.
    fn choice(&self, n: usize, text: &str, writes: &str, width: usize) -> Vec<CardLine> {
        let mark = if self.row == n { "> " } else { "  " };
        if writes.is_empty() {
            return vec![line(format!("{mark}{text}"), Kind::Choice(n))];
        }
        let one = format!("{mark}{text}   {writes}");
        if one.width() <= width {
            return vec![line(one, Kind::Choice(n))];
        }
        let mut out = vec![line(format!("{mark}{text}"), Kind::Choice(n))];
        out.extend(
            wrap(writes, width.saturating_sub(4))
                .into_iter()
                .map(|t| line(format!("    {t}"), Kind::Choice(n))),
        );
        out
    }
}

/// The gap between the grid's columns.
const GRID_GAP: usize = 2;

/// The keys card's body: three columns when they fit, one line per key when they do not.
///
/// The prose cell on the second row spans columns two and three, which is what lets the grid
/// hold a 105-column sentence's worth of keys in 92 columns.
fn keys_grid(app: &App, width: usize) -> Vec<String> {
    let cells: Vec<[String; 3]> = KEY_CELLS
        .iter()
        .map(|row| [cell(app, row[0]), cell(app, row[1]), cell(app, row[2])])
        .collect();
    let col = |i: usize| -> usize {
        cells
            .iter()
            // The spanning cell is not allowed to set a column width; it is measured
            // against the two columns together, below.
            .filter(|r| !(i > 0 && r[2].is_empty()))
            .map(|r| r[i].width())
            .max()
            .unwrap_or(0)
    };
    let (w0, w1, w2) = (col(0), col(1), col(2));
    let span = cells
        .iter()
        .filter(|r| r[2].is_empty())
        .map(|r| r[1].width())
        .max()
        .unwrap_or(0);
    let right = (w1 + GRID_GAP + w2).max(span);
    let total = w0 + GRID_GAP + right;
    if total > width {
        // One key per line: the same cells, in the same reading order.
        return cells
            .iter()
            .flat_map(|r| r.iter())
            .filter(|c| !c.is_empty())
            .cloned()
            .collect();
    }
    cells
        .iter()
        .map(|r| {
            let mut out = String::new();
            out.push_str(&r[0]);
            out.push_str(&" ".repeat(w0 + GRID_GAP - r[0].width()));
            if r[2].is_empty() {
                out.push_str(&r[1]);
            } else {
                out.push_str(&r[1]);
                out.push_str(&" ".repeat(w1 + GRID_GAP - r[1].width()));
                out.push_str(&r[2]);
            }
            out.trim_end().to_owned()
        })
        .collect()
}

/// Word-wrap `text` to `width` columns. A word longer than the width gets its own line
/// rather than being cut: these are sentences, and the only long word in them is a path.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_owned()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.width() + 1 + word.width() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            out.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::Loading;
    use crate::tui::app::testfix::{meta, pile, pile_event};
    use crate::tui::herdr::Scope;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use lastcall_engine::config::write::NO_CONFIG_DIR;
    use lastcall_engine::scan::Pile;
    use lastcall_testkit::tmp::TempDir;
    use std::collections::{BTreeMap, BTreeSet};

    // ---- the harness ------------------------------------------------------------------

    /// An environment that resolves a config file under `dir` and nothing else. Built from
    /// `Env::empty`, so no variable of the machine running the test can reach it.
    fn env_at(dir: &Path) -> Env {
        Env::empty(dir).with_var(
            "XDG_CONFIG_HOME",
            dir.join("config").to_str().expect("utf-8 temp dir"),
        )
    }

    fn config_file(dir: &Path) -> PathBuf {
        dir.join("config").join("lastcall").join("config.toml")
    }

    fn write_config(dir: &Path, text: &str) {
        let path = config_file(dir);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write config");
    }

    /// An app past the launch hold with three roots that have pending rows.
    fn app_at(w: u16, h: u16) -> App {
        let mut app = App::new();
        app.handle(Action::Resize(w, h));
        app.sync_roots(vec![meta("alpha"), meta("beta"), meta("notes")]);
        for name in ["alpha", "beta", "notes"] {
            app.apply(pile_event(name, pile(name)));
        }
        app
    }

    /// An app with `empty` roots that have nothing pending and `pending` that do.
    fn app_with_empties(empty: usize, pending: usize) -> App {
        let mut app = App::new();
        app.handle(Action::Resize(100, 30));
        let names: Vec<String> = (0..empty + pending).map(|i| format!("r{i:02}")).collect();
        app.sync_roots(names.iter().map(|n| meta(n)).collect());
        for (i, name) in names.iter().enumerate() {
            let p = if i < pending {
                pile("alpha")
            } else {
                Pile::default()
            };
            app.apply(pile_event(name, p));
        }
        app
    }

    /// A live herdr link that derived a scope and is honouring it: every part of the herdr
    /// card's condition, so a test can take one away at a time.
    fn following(app: &mut App) {
        app.herdr.link = Link::Connected {
            version: "0.8.2".to_owned(),
        };
        app.herdr.scope = Some(Scope {
            label: "W".to_owned(),
            roots: BTreeSet::from([PathBuf::from("/W/alpha")]),
        });
        app.herdr.scoped = true;
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    // ---- the marker -------------------------------------------------------------------

    /// The whole "has this been seen?" record: absent is no, present and parsable is yes.
    #[test]
    fn tour_marker_round_trips_through_the_state_directory() {
        let dir = TempDir::new("lc-tour-marker");
        assert!(!shown_before(dir.path()), "nothing written yet");
        let now = UNIX_EPOCH + std::time::Duration::from_secs(1_789_344_000);
        write_marker(dir.path(), now, "0.1.0").expect("write");
        assert!(shown_before(dir.path()));
        let text = std::fs::read_to_string(marker_path(dir.path())).expect("read");
        let marker: Marker = serde_json::from_str(&text).expect("parses");
        assert_eq!(
            marker,
            Marker {
                shown_at: 1_789_344_000,
                version: "0.1.0".to_owned(),
            }
        );
        assert!(
            !marker_path(dir.path()).with_extension("json.tmp").exists(),
            "the temp file is renamed, not left behind"
        );
    }

    /// The e2e tier's harness writes this marker into every isolated state dir so a scene
    /// that never thought about the welcome starts on the review screen. It is below this
    /// crate in the dependency graph and spells the name itself; this is the seam.
    #[test]
    fn tour_marker_is_the_file_the_harness_writes() {
        assert_eq!(MARKER_FILE, lastcall_testkit::pty_tui::MARKER_FILE);
        let marker: Marker = serde_json::from_str(lastcall_testkit::pty_tui::MARKER_SEEN)
            .expect("the harness writes a marker this build can read");
        assert_eq!(marker.version, "0.0.0-testkit");
    }

    /// A marker we cannot read is not a record: the welcome opens once more and writes a
    /// marker we can. Refusing to help a new user over a file we wrote and then broke is
    /// the worse failure.
    #[test]
    fn tour_marker_that_does_not_parse_shows_the_welcome_again() {
        let dir = TempDir::new("lc-tour-marker-junk");
        std::fs::write(marker_path(dir.path()), b"{ not json").expect("write");
        assert!(!shown_before(dir.path()));
        write_marker(dir.path(), SystemTime::now(), "0.1.0").expect("write");
        assert!(shown_before(dir.path()), "and then it is a record again");
    }

    /// `--tour` ignores the marker; without it the marker is the whole decision.
    #[test]
    fn tour_force_ignores_the_marker() {
        let dir = TempDir::new("lc-tour-force");
        let env = env_at(dir.path());
        assert!(
            Plan::new(false, env.clone(), dir.path().to_owned()).owed(),
            "no marker: the welcome is owed"
        );
        write_marker(dir.path(), SystemTime::now(), "0.1.0").expect("write");
        assert!(!Plan::new(false, env.clone(), dir.path().to_owned()).owed());
        assert!(
            Plan::new(true, env, dir.path().to_owned()).owed(),
            "--tour shows it again"
        );
    }

    /// Every dismissal writes the marker, and only the first one does the work.
    #[test]
    fn tour_dismissal_writes_the_marker_once() {
        let dir = TempDir::new("lc-tour-dismiss");
        let mut plan = Plan::new(false, env_at(dir.path()), dir.path().to_owned());
        let first = UNIX_EPOCH + std::time::Duration::from_secs(1_789_344_000);
        plan.dismissed(first);
        assert!(!plan.owed(), "dismissed");
        plan.dismissed(first + std::time::Duration::from_secs(99));
        let marker: Marker =
            serde_json::from_str(&std::fs::read_to_string(marker_path(dir.path())).expect("read"))
                .expect("parses");
        assert_eq!(
            marker.shown_at, 1_789_344_000,
            "the second dismissal wrote nothing"
        );
    }

    // ---- the fixed keys ---------------------------------------------------------------

    /// The keys the footer promises, and nothing else.
    #[test]
    fn tour_action_maps_the_fixed_keys_and_ignores_the_rest() {
        let keymap = Keymap::defaults();
        for (event, want) in [
            (key(KeyCode::Enter), Some(Action::Tour(TourKey::Next))),
            (key(KeyCode::Up), Some(Action::Tour(TourKey::Up))),
            (key(KeyCode::Down), Some(Action::Tour(TourKey::Down))),
            (key(KeyCode::Char('k')), Some(Action::Tour(TourKey::Up))),
            (key(KeyCode::Char('j')), Some(Action::Tour(TourKey::Down))),
            (key(KeyCode::Esc), Some(Action::Tour(TourKey::Skip))),
            (key(KeyCode::Char('q')), Some(Action::Tour(TourKey::Skip))),
            // Bound to `accept`, `help` and `hide_empty` outside the tour: all swallowed.
            (key(KeyCode::Char('a')), None),
            (key(KeyCode::Char('?')), None),
            (key(KeyCode::Char('t')), None),
            (key(KeyCode::Tab), None),
            // The escape hatch a text modal keeps open, on the same terms.
            (ctrl('c'), Some(Action::Quit)),
        ] {
            assert_eq!(tour_action(&event, &keymap), want, "{event:?}");
        }
    }

    /// A quit bound to a printable key skips instead of quitting: nothing printable may
    /// leave an unanswered question, and `q` is what the footer offered.
    #[test]
    fn tour_action_turns_a_printable_quit_binding_into_a_skip() {
        let keymap = Keymap::from_config(&BTreeMap::from([(
            "quit".to_owned(),
            lastcall_engine::config::KeySpecs::One("x".to_owned()),
        )]))
        .expect("keymap");
        assert_eq!(
            tour_action(&key(KeyCode::Char('x')), &keymap),
            Some(Action::Tour(TourKey::Skip))
        );
        assert_eq!(
            tour_action(&key(KeyCode::Char('q')), &keymap),
            Some(Action::Tour(TourKey::Skip)),
            "q is the footer's promise whatever the keymap says"
        );
    }

    // ---- when it opens ----------------------------------------------------------------

    /// The launch hold, the scope verdict, and a frame too small each hold it back, and
    /// nothing is written while they do, so the welcome survives to the next launch.
    #[test]
    fn tour_waits_for_a_settled_frame_with_room_to_read_it() {
        let dir = TempDir::new("lc-tour-due");
        let plan = Plan::new(false, env_at(dir.path()), dir.path().to_owned());
        let mut app = app_at(100, 30);
        assert!(plan.due(&app), "settled, and large enough");

        app.loading = Some(Loading {
            started: std::time::Instant::now(),
            checked: BTreeMap::new(),
            scanned: false,
        });
        assert!(!plan.due(&app), "the launch hold is still on");
        app.loading = None;

        app.herdr.scope_pending = true;
        assert!(!plan.due(&app), "no scope verdict yet");
        app.herdr.scope_pending = false;

        for (w, h) in [(MIN_COLS - 1, MIN_ROWS), (MIN_COLS, MIN_ROWS - 1)] {
            app.handle(Action::Resize(w, h));
            assert!(!plan.due(&app), "{w}x{h} is too small to read it");
        }
        app.handle(Action::Resize(MIN_COLS, MIN_ROWS));
        assert!(plan.due(&app), "the smallest frame it opens on");

        assert!(
            !marker_path(dir.path()).exists(),
            "nothing is written before it opens"
        );
    }

    /// Opened twice is not a thing: `due` is false the moment the overlay is up.
    #[test]
    fn tour_does_not_open_over_itself() {
        let dir = TempDir::new("lc-tour-twice");
        let mut plan = Plan::new(false, env_at(dir.path()), dir.path().to_owned());
        let mut app = app_at(100, 30);
        plan.open(&mut app);
        assert!(app.tour.is_some());
        assert!(!plan.due(&app));
    }

    // ---- which cards ------------------------------------------------------------------

    /// The keys card is the tour's floor: it is always there, alone when nothing else
    /// applies.
    #[test]
    fn tour_always_has_the_keys_card_and_nothing_it_has_no_reason_for() {
        let dir = TempDir::new("lc-tour-cards");
        let mut plan = Plan::new(false, env_at(dir.path()), dir.path().to_owned());
        let mut app = app_at(100, 30);
        plan.open(&mut app);
        let tour = app.tour.as_ref().expect("open");
        assert_eq!(tour.cards, vec![Card::Keys]);
    }

    /// F5: the herdr card needs a live link, a derived scope, that scope honoured, and a
    /// file that has not already answered. Take any one away and the card is about nothing.
    #[test]
    fn tour_herdr_card_needs_every_part_of_its_condition() {
        let dir = TempDir::new("lc-tour-herdr");
        let herdr = Card::Herdr {
            version: "0.8.2".to_owned(),
        };

        let cards = |dir: &Path, edit: fn(&mut App)| -> Vec<Card> {
            let mut plan = Plan::new(false, env_at(dir), dir.to_owned());
            let mut app = app_at(100, 30);
            following(&mut app);
            edit(&mut app);
            plan.open(&mut app);
            app.tour.expect("open").cards
        };

        assert!(
            cards(dir.path(), |_| {}).contains(&herdr),
            "every part holds"
        );
        for (why, edit) in [
            (
                "no live link",
                (|a: &mut App| a.herdr.link = Link::Off) as fn(&mut App),
            ),
            ("no scope derived", |a: &mut App| a.herdr.scope = None),
            ("scope not honoured", |a: &mut App| a.herdr.scoped = false),
        ] {
            assert!(
                !cards(dir.path(), edit).contains(&herdr),
                "{why}: the card would be about nothing"
            );
        }

        // The fourth part: the file already says what the card would ask.
        let answered = TempDir::new("lc-tour-herdr-set");
        write_config(answered.path(), "[herdr]\nscope = \"all\"\n");
        assert!(
            !cards(answered.path(), |_| {}).contains(&herdr),
            "the file already answered it"
        );
    }

    /// The empty card needs enough empty repositories to be worth a question, the setting
    /// not already on for the session, and a file that has not already answered.
    #[test]
    fn tour_empty_card_needs_ten_empty_repositories_and_an_unanswered_file() {
        let dir = TempDir::new("lc-tour-empty");
        let card = Card::Empty {
            empty: 12,
            total: 14,
        };

        let cards = |dir: &Path, empty: usize, edit: fn(&mut App)| -> Vec<Card> {
            let mut plan = Plan::new(false, env_at(dir), dir.to_owned());
            let mut app = app_with_empties(empty, 2);
            edit(&mut app);
            plan.open(&mut app);
            app.tour.expect("open").cards
        };

        assert!(cards(dir.path(), 12, |_| {}).contains(&card));
        assert!(
            !cards(dir.path(), EMPTY_CARD_MIN - 1, |_| {})
                .iter()
                .any(|c| matches!(c, Card::Empty { .. })),
            "under {EMPTY_CARD_MIN} empty repositories is not worth a question"
        );
        assert!(
            !cards(dir.path(), 12, |a: &mut App| a.hide_empty = true)
                .iter()
                .any(|c| matches!(c, Card::Empty { .. })),
            "already hidden for the session"
        );

        let answered = TempDir::new("lc-tour-empty-set");
        write_config(answered.path(), "hide_empty_repos = false\n");
        assert!(
            !cards(answered.path(), 12, |_| {})
                .iter()
                .any(|c| matches!(c, Card::Empty { .. })),
            "the file already answered it, with either value"
        );
    }

    /// The empty card's title counts what is on screen, not what is on disk.
    #[test]
    fn tour_empty_card_counts_the_roots_that_are_listed() {
        let dir = TempDir::new("lc-tour-count");
        let mut plan = Plan::new(false, env_at(dir.path()), dir.path().to_owned());
        let mut app = app_with_empties(12, 2);
        plan.open(&mut app);
        let tour = app.tour.as_ref().expect("open");
        assert!(
            tour.cards.contains(&Card::Empty {
                empty: 12,
                total: 14
            }),
            "cards were {:?}",
            tour.cards
        );
    }

    // ---- the card as text -------------------------------------------------------------

    /// Every card fits the smallest frame the overlay opens on, with a line to spare for
    /// the box: nothing is truncated and nothing is cut off the bottom.
    #[test]
    fn tour_every_card_fits_the_smallest_frame_it_opens_on() {
        let app = app_at(MIN_COLS, MIN_ROWS);
        let width = usize::from(MIN_COLS) - 4;
        let height = usize::from(MIN_ROWS) - 2;
        for cards in [
            vec![Card::Keys],
            vec![Card::Herdr {
                version: "0.8.2".to_owned(),
            }],
            vec![Card::Empty {
                empty: 12,
                total: 14,
            }],
        ] {
            let tour = Tour::new(cards);
            let lines = tour.lines(&app, width, height);
            for l in &lines {
                assert!(
                    l.text.width() <= width,
                    "{:?} is wider than {width} columns",
                    l.text
                );
            }
            // The painter drops blank rows before it clips; the non-blank ones must fit.
            let solid = lines.iter().filter(|l| !l.text.is_empty()).count();
            assert!(
                solid <= height,
                "{solid} lines of text do not fit {height} rows: {:?}",
                tour.card()
            );
            // Whatever had to go, the footer's keys and the way to the rest of them stay.
            let text: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
            assert!(text.contains(&KEYS_FOOTER) || text.contains(&CHOICE_FOOTER));
            if tour.card() == &Card::Keys {
                assert!(
                    text.iter().any(|t| t.ends_with("every key, any time")),
                    "the way to all of them is the last thing to go: {text:?}"
                );
            }
        }
    }

    /// A choice row that will not fit on one line keeps its sentence and puts what it
    /// writes underneath, indented, still the same clickable row.
    #[test]
    fn tour_choice_puts_what_it_writes_on_its_own_line_when_narrow() {
        let app = app_at(80, 24);
        let tour = Tour::new(vec![Card::Empty {
            empty: 12,
            total: 14,
        }]);
        let wide: Vec<String> = tour
            .lines(&app, 120, 40)
            .iter()
            .filter(|l| l.kind == Kind::Choice(1))
            .map(|l| l.text.clone())
            .collect();
        assert_eq!(
            wide,
            vec![
                "  Start with the empty ones hidden, and remember that   (writes hide_empty_repos = true)"
                    .to_owned()
            ],
            "one line when there is room"
        );
        let narrow: Vec<String> = tour
            .lines(&app, 72, 40)
            .iter()
            .filter(|l| l.kind == Kind::Choice(1))
            .map(|l| l.text.clone())
            .collect();
        assert_eq!(
            narrow,
            vec![
                "  Start with the empty ones hidden, and remember that".to_owned(),
                "    (writes hide_empty_repos = true)".to_owned(),
            ]
        );
    }

    /// F15: the keys card is rendered from the effective keymap, so a user with a `[keys]`
    /// table is told about their own bindings on their first launch, and the **first**
    /// spelling only, because `n / ]` is a second thing to learn at the worst moment.
    #[test]
    fn tour_keys_card_shows_the_keymap_in_force() {
        let mut app = app_at(100, 30);
        let lines = |app: &App| -> String {
            Tour::new(vec![Card::Keys])
                .lines(app, 92, 40)
                .iter()
                .map(|l| l.text.clone())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let before = lines(&app);
        assert!(
            before.contains("a  accept the hunk under the cursor"),
            "{before}"
        );
        assert!(
            before.contains("n / p  next and previous hunk"),
            "the first spelling of each, not n / ]: {before}"
        );
        for (name, specs) in &mut app.keymap {
            if name == "accept" {
                *specs = vec!["F5".to_owned()];
            }
        }
        let after = lines(&app);
        assert!(
            after.contains("F5  accept the hunk under the cursor"),
            "{after}"
        );
    }

    /// The selected row is the one with the marker, and it moves with the arrows.
    #[test]
    fn tour_marks_the_selected_choice_row() {
        let app = app_at(100, 30);
        let mut tour = Tour::new(vec![Card::Empty {
            empty: 12,
            total: 14,
        }]);
        let marks = |tour: &Tour| -> Vec<String> {
            tour.lines(&app, 92, 40)
                .iter()
                .filter(|l| matches!(l.kind, Kind::Choice(_)))
                .map(|l| l.text.chars().take(2).collect())
                .collect()
        };
        assert_eq!(marks(&tour), vec!["> ".to_owned(), "  ".to_owned()]);
        tour.row = 1;
        assert_eq!(marks(&tour), vec!["  ".to_owned(), "> ".to_owned()]);
        assert_eq!(tour.rows(), 2, "a choice card has two rows");
        assert_eq!(
            Tour::new(vec![Card::Keys]).rows(),
            0,
            "the keys card has none"
        );
    }

    /// A write that failed says so and prints the line to type, table header and all, in
    /// place of the footer's keys.
    #[test]
    fn tour_failed_write_shows_the_line_to_add_by_hand() {
        let app = app_at(100, 30);
        let mut tour = Tour::new(vec![Card::Herdr {
            version: "0.8.2".to_owned(),
        }]);
        tour.failed = Some(failure_line("/c/config.toml", "read-only file system"));
        let text: Vec<String> = tour
            .lines(&app, 92, 40)
            .iter()
            .filter(|l| l.kind == Kind::Footer)
            .map(|l| l.text.clone())
            .collect();
        assert_eq!(
            text,
            vec![
                "could not write /c/config.toml: read-only file system. Add this line yourself:"
                    .to_owned(),
                "    [herdr]".to_owned(),
                "    scope = \"all\"".to_owned(),
            ]
        );
        assert!(
            !text.iter().any(|l| l.contains("skip the rest")),
            "the reason replaces the keys, it does not crowd in beside them"
        );
    }

    /// Nowhere to write is a sentence too, without a path in it.
    #[test]
    fn tour_failure_without_a_path_still_reads() {
        assert_eq!(
            failure_line("", NO_CONFIG_DIR),
            "could not write the config file: no configuration directory (set XDG_CONFIG_HOME or HOME). Add this line yourself:"
        );
    }

    // ---- the write ---------------------------------------------------------------------

    /// The whole write path, end to end: the tour's choice creates the file with the
    /// provenance comment and one key, and a second choice edits the file it just made.
    #[test]
    fn tour_write_creates_the_config_file_and_then_edits_it() {
        let dir = TempDir::new("lc-tour-write");
        let mut plan = Plan::new(false, env_at(dir.path()), dir.path().to_owned());
        let at = UNIX_EPOCH + std::time::Duration::from_secs(1_789_344_000);
        plan.write(Setting::HideEmptyRepos, at).expect("created");
        plan.write(Setting::HerdrScopeAll, at).expect("edited");
        assert_eq!(
            std::fs::read_to_string(config_file(dir.path())).expect("read"),
            "# written by lastcall's first-launch tour on 2026-09-14\n\
             hide_empty_repos = true\n\
             \n\
             [herdr]\n\
             scope = \"all\"\n"
        );
    }

    /// A file the tour cannot write comes back as the sentence the card shows, with the
    /// path in it.
    #[test]
    fn tour_write_that_fails_comes_back_as_the_cards_sentence() {
        let dir = TempDir::new("lc-tour-write-fail");
        write_config(dir.path(), "herdr = 3\n");
        let mut plan = Plan::new(false, env_at(dir.path()), dir.path().to_owned());
        let message = plan
            .write(Setting::HerdrScopeAll, SystemTime::now())
            .expect_err("herdr is not a table");
        assert!(
            message.starts_with("could not write ")
                && message.contains("config.toml")
                && message.ends_with("Add this line yourself:"),
            "{message}"
        );
        assert_eq!(
            std::fs::read_to_string(config_file(dir.path())).expect("read"),
            "herdr = 3\n",
            "the file it could not write is left alone"
        );
    }
}
