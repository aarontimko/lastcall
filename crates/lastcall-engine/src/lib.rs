//! lastcall engine: configuration, the herdr client, and (from Phase 2) the review ledger.
//!
//! This crate never contains terminal code. Everything here is drivable from tests with no
//! terminal, no network, and no access to the real user environment: every path and
//! environment read goes through [`env::Env`], which tests construct explicitly.

pub mod config;
pub mod count;
pub mod engine;
pub mod env;
pub mod flags;
pub mod git;
pub mod headstate;
#[cfg(feature = "herdr")]
pub mod herdr;
pub mod hunks;
pub mod index;
pub mod ledger;
pub mod ops;
pub mod paths;
pub mod restore;
pub mod roots;
pub mod scan;
pub mod status;
pub mod store;
pub mod upstream;
pub mod watcher;

/// One-line test seams (Phase 13 deliverable B). A seam is a thread-local slot holding a
/// closure the production path fires at a named point; with no closure installed it is a
/// predicate test and a call that does nothing, and none of it exists outside `cfg(test)`.
///
/// The slot is **emptied while the closure runs**, so a hook that drives a second engine
/// through the same point does not re-enter itself (and `RefCell` never double-borrows);
/// it is put back afterwards unless the closure installed a new one.
#[cfg(test)]
pub(crate) mod testhook {
    use std::cell::RefCell;

    /// The installed closure, or nothing.
    type Slot<A> = RefCell<Option<Box<dyn FnMut(A)>>>;

    pub(crate) struct TestHook<A: 'static> {
        slot: Slot<A>,
    }

    impl<A: 'static> TestHook<A> {
        pub(crate) const fn new() -> Self {
            Self {
                slot: RefCell::new(None),
            }
        }

        pub(crate) fn set(&self, f: impl FnMut(A) + 'static) {
            *self.slot.borrow_mut() = Some(Box::new(f));
        }

        pub(crate) fn clear(&self) {
            *self.slot.borrow_mut() = None;
        }

        pub(crate) fn fire(&self, arg: A) {
            let taken = self.slot.borrow_mut().take();
            if let Some(mut f) = taken {
                f(arg);
                let mut slot = self.slot.borrow_mut();
                if slot.is_none() {
                    *slot = Some(f);
                }
            }
        }
    }
}
