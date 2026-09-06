//! The Phase 3 Ratatui app (kickoff `docs/spec/92-phase3-kickoff.md`).
//!
//! Layering, worker by worker:
//! - `term`   — raw mode, alternate screen, mouse capture, panic-safe restore, file-only tracing.
//! - `app`    — the pure state (`App`) and its reducers `apply` / `handle` / `sync_roots`.
//! - `editor` — pure `$VISUAL`/`$EDITOR` resolution and the per-editor "open at line" argv.
//! - `input`  — the `Action` vocabulary and `DEFAULT_KEYMAP`, `to_action(Event, &Keymap)`
//!   and `Keymap::from_config` (the `[keys]` table).
//! - `textbuf` — the editable text buffer the note modal and the inline editor share.
//! - `render` — `render(&App, frame) -> HitMap`, plus `styles(&Buffer)` for the snapshot tier.
//! - `herdr`  — the socket-free boundary to the herdr client: `HerdrUpdate`, `HerdrView`,
//!   the pure `derive`, and the one task that talks to the socket (Phase 5).
//! - `run`    — the event loop: `run(engine, timings, keymap)`; the CLI wiring for bare
//!   `lastcall` / `lastcall tui` is `commands/tui.rs` in the binary.
//!
//! Invariant 9 holds throughout: the TUI reads engine piles only; it never touches a file, a
//! ledger or git, and it holds no review state of its own.

pub mod app;
pub mod editor;
pub mod herdr;
pub mod input;
pub mod render;
pub mod run;
pub mod term;
pub mod textbuf;
