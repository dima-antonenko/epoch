//! Teaching implementations of safe memory reclamation and the lock-free
//! collections built on top of them.
//!
//! A reclamation scheme answers when the memory of an unlinked node may be
//! destroyed: another thread may still be reading its fields. That question
//! is separate from ABA. The pointer generation in [`tagged`] closes ABA on
//! its own: a CAS must not treat an address that was removed and inserted
//! again as an unchanged link.
//!
//! The public collections are safe. `unsafe` stays inside the schemes and
//! inside the structures.

#![deny(unsafe_op_in_unsafe_fn)]

pub(crate) mod atomic;
pub mod map;
pub(crate) mod pad;
pub mod queue;
pub mod reclaim;
pub(crate) mod retired;
pub mod stack;
pub mod tagged;

pub use map::{EpochMap, HazardMap};
pub use queue::{DeferredQueue, EpochQueue, HazardQueue, RcuQueue};
pub use stack::{DeferredStack, EpochStack, HazardStack, RcuStack};
