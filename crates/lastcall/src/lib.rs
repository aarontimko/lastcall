//! Library half of the `lastcall` binary crate: the Ratatui TUI (Phase 3). It exists so the
//! `tests/` tier can drive `tui::App` and `tui::render` against a `TestBackend`; the CLI
//! (`src/main.rs`) stays the only place that parses arguments and picks a command.

pub mod tui;
