//! The input vocabulary (kickoff deliverable 5, worker 3a's half): the [`Action`] enum and
//! the [`DEFAULT_KEYMAP`] table. The crossterm translation `to_action(Event, &Keymap)` and
//! `Keymap::from_config(&Config)` (the `[keys]` override with its notices) are worker 3b's.
//!
//! Key spec grammar (for 3b): optional `ctrl-` / `alt-` / `shift-` prefixes, then a single
//! character or a named key (`up down left right pageup pagedown home end enter esc tab
//! backtab space backspace delete f1..f12`), case-insensitive.

/// Everything the app can be asked to do. Keys, mouse gestures and the tick all become one
/// of these before they touch [`super::app::App`], so the keyboard and mouse paths are
/// provably equivalent (the parity tests) and the reducer never sees a crossterm type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nav focus: previous entry. Diff focus: scroll up one line.
    NavUp,
    /// Nav focus: next entry. Diff focus: scroll down one line.
    NavDown,
    /// Nav focus: a page of entries up. Diff focus: a page of lines up.
    NavPageUp,
    /// Nav focus: a page of entries down. Diff focus: a page of lines down.
    NavPageDown,
    /// Focus the diff for the selected row/group; on a root entry, select its first row.
    Open,
    /// Close the help overlay if open, else return focus to the nav. Never quits.
    Back,
    FocusToggle,
    HunkNext,
    HunkPrev,
    /// Scroll the diff up `n` lines (the wheel; the loop sends `NavUp` when the pointer is
    /// over the nav instead).
    ScrollUp(u16),
    ScrollDown(u16),
    ToggleFullPaths,
    ToggleRemote,
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
}

/// Action name (the `[keys]` config key) → default key specs, in help-overlay order.
pub const DEFAULT_KEYMAP: &[(&str, &[&str])] = &[
    ("nav_up", &["up", "k"]),
    ("nav_down", &["down", "j"]),
    ("nav_page_up", &["pageup", "b"]),
    ("nav_page_down", &["pagedown", "space"]),
    ("open", &["enter", "l"]),
    ("back", &["esc", "h"]),
    ("focus_toggle", &["tab"]),
    ("hunk_next", &["n", "]"]),
    ("hunk_prev", &["p", "["]),
    ("toggle_full_paths", &["f"]),
    ("toggle_remote", &["o"]),
    ("refresh", &["r"]),
    ("help", &["?"]),
    ("quit", &["q", "ctrl-c"]),
];

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
            "toggle_full_paths" => Action::ToggleFullPaths,
            "toggle_remote" => Action::ToggleRemote,
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
            "toggle_full_paths" => "full paths",
            "toggle_remote" => "show org/repo",
            "refresh" => "rescan now",
            "help" => "this help",
            "quit" => "quit",
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            (Action::ScrollUp(3), "wheel"),
            (Action::ScrollDown(3), "wheel"),
            (Action::ToggleFullPaths, "key"),
            (Action::ToggleRemote, "key"),
            (Action::Refresh, "key"),
            (Action::Help, "key"),
            (Action::Quit, "key"),
            (Action::Press(1, 1), "mouse"),
            (Action::Drag(1, 1), "mouse"),
            (Action::Release, "mouse"),
            (Action::Resize(80, 24), "terminal"),
            (Action::Tick, "timer"),
        ];
        for (action, source) in table {
            match *source {
                "key" => assert!(by_key.contains(action), "{action:?} has no default key"),
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
            | Action::ScrollUp(_)
            | Action::ScrollDown(_)
            | Action::ToggleFullPaths
            | Action::ToggleRemote
            | Action::Refresh
            | Action::Help
            | Action::Quit
            | Action::Press(_, _)
            | Action::Drag(_, _)
            | Action::Release
            | Action::Resize(_, _)
            | Action::Tick => 21,
        };
        assert_eq!(table.len(), 21);
    }
}
