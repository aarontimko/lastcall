//! The herdr client (docs/spec/00-spec.md §5 frozen v1.0, §6.6).
//!
//! Nothing in the review engine depends on this module; it compiles out behind the `herdr`
//! feature and is referenced only by the binary.
//!
//! - [`wire`]: every request, response, event, and payload type we consume, hand-checked
//!   against herdr's `herdr-api.schema.json`. Unknown fields are tolerated everywhere here.
//! - [`transport`]: newline-JSON over a Unix socket behind the [`transport::Transport`] trait.
//! - [`discovery`]: session discovery in herdr's own precedence.
//! - [`guard`]: the ping-then-compare protocol guard.
//! - [`client`]: the state machine — events are hints, snapshots are truth (invariant 9).

pub mod client;
pub mod discovery;
pub mod guard;
pub mod transport;
pub mod wire;

/// Bracketed-paste opener (`ESC [ 200 ~`).
///
/// A terminal application that has enabled bracketed paste treats everything between the
/// markers as *pasted text* — it lands in the input buffer and is not submitted, however
/// many newlines it holds. The **application** interprets them, never the tty line
/// discipline (F7), so `cat` or a non-interactive shell will happily run each line: the
/// staging proof needs a real interactive shell with `zle`'s `bracketed-paste` widget.
pub const BRACKETED_PASTE_START: &str = "\u{1b}[200~";
/// Bracketed-paste closer (`ESC [ 201 ~`).
///
/// The reason [`crate::flags::export`] renders every control byte in caret form: a `201~`
/// sequence inside the payload would end the paste early and let the remainder arrive as
/// keystrokes — which a shell would run.
pub const BRACKETED_PASTE_END: &str = "\u{1b}[201~";

pub use client::{
    Cache, Client, ClientHandle, ClientOptions, ClientTimings, HerdrEvent, ResyncTarget,
};
pub use guard::{Compat, SUPPORTED_PROTOCOL};
pub use transport::{EventStream, SocketTransport, Transport, TransportError};
