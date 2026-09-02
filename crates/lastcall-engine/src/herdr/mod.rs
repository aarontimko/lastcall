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

pub use client::{
    Cache, Client, ClientHandle, ClientOptions, ClientTimings, HerdrEvent, ResyncTarget,
};
pub use guard::{Compat, SUPPORTED_PROTOCOL};
pub use transport::{EventStream, SocketTransport, Transport, TransportError};
