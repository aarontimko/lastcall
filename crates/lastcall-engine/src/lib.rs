//! lastcall engine: configuration, the herdr client, and (from Phase 2) the review ledger.
//!
//! This crate never contains terminal code. Everything here is drivable from tests with no
//! terminal, no network, and no access to the real user environment: every path and
//! environment read goes through [`env::Env`], which tests construct explicitly.

pub mod config;
pub mod env;
pub mod git;
#[cfg(feature = "herdr")]
pub mod herdr;
pub mod hunks;
pub mod index;
pub mod ledger;
pub mod paths;
pub mod scan;
pub mod store;
