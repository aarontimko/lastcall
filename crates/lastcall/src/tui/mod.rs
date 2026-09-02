//! The Phase 3 Ratatui app (kickoff `docs/spec/92-phase3-kickoff.md`).
//!
//! Layering, worker by worker:
//! - `term`   — raw mode, alternate screen, mouse capture, panic-safe restore, file-only tracing.
//! - `app`    — the pure state (`App`) and its reducers `apply` / `handle` / `sync_roots`.
//! - `input`  — the `Action` vocabulary and `DEFAULT_KEYMAP` (worker 3a); `to_action` and
//!   `Keymap::from_config` are worker 3b's.
//! - `render` — `render(&App, frame) -> HitMap`, plus `styles(&Buffer)` for the snapshot tier.
//! - `run`    — the event loop and the `lastcall` (no-subcommand) wiring: worker 3b.
//!
//! Invariant 9 holds throughout: the TUI reads engine piles only; it never touches a file, a
//! ledger or git, and it holds no review state of its own.

pub mod app;
pub mod input;
pub mod render;
pub mod term;
