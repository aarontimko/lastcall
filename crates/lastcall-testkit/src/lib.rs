//! Test-only support for lastcall.
//!
//! A normal library crate that product crates list under `[dev-dependencies]` only
//! (`cargo tree -e normal -p lastcall -p lastcall-engine | grep -c testkit` prints 0).
//!
//! - [`mock_herdr`]: a scripted herdr server, as an in-memory `Transport` (state-machine unit
//!   tests under paused tokio time) and as a real Unix-socket server (transport tests and
//!   `just probe-hello`).
//! - [`herdr_spawn`]: the isolated real-herdr spawner (docs/spec/00-spec.md §5.10).
//! - [`herdr_schema`]: the consumed-surface projection of `herdr api schema --json` behind the
//!   weekly compat check (`just herdr-schema-fixture`, `.github/workflows/herdr-compat.yml`).
//! - [`fixture_repo`]: deterministic fixture git repositories with a local bare origin.
//! - [`fixture_parent`]: the golden's three-root parent dir (two repos + a draft dir) with
//!   first sight done before the history operations; `just probe-status` / `probe-watch`.
//! - [`engine`]: open an engine over a fixture, `assert_pile!` in the harness's format, and
//!   the SIGKILL fault injector for E1.
//! - [`json_lines`]: a small synchronous newline-JSON reader over `UnixStream`, adapted from
//!   herdr's test suite (see NOTICE).
//! - [`pty_tui`]: the TUI PTY harness (Phase 3): a binary inside a real pseudo-terminal, a
//!   `vt100` screen plus a raw transcript, `wait_for` polling, clicks and resizes.

pub mod engine;
pub mod fixture_parent;
pub mod fixture_repo;
pub mod herdr_schema;
pub mod herdr_spawn;
pub mod json_lines;
pub mod mock_herdr;
pub mod pty_tui;
pub mod tmp;
