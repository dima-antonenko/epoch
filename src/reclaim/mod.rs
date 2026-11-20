//! Four schemes for deferred memory reclamation.
//!
//! Each scheme has its own domain. The domain belongs to a collection, not
//! to the process: tests and neighboring structures do not share limbo. A
//! domain admits at most [`MAX_THREADS`] threads. A thread claims a slot
//! with a CAS and remembers the index in TLS. When the thread exits, the
//! TLS destructor clears the slot and hands the local limbo list to the
//! orphan list. Otherwise a departed thread would pin epoch, RCU, or
//! deferred forever: their barrier looks at every occupied slot.
//!
//! With the `loom` feature the TLS destructor does not touch atomics. The
//! model has already finished by then, so `free_all_for_shutdown` drops the
//! registration while the model is still running.
//!
//! [`INACTIVE`] means “the thread is in the domain but not in a section”.
//! For epoch and deferred that is `usize::MAX`, a value larger than any
//! live counter. An occupied slot with that label does not hold reclamation
//! back.

mod deferred;
mod epoch;
mod hazard;
mod rcu;

pub use deferred::{DeferredDomain, DeferredGuard};
pub use epoch::{EpochDomain, EpochGuard};
pub use hazard::{HazardDomain, Shield};
pub use rcu::{RcuDomain, RcuGuard};

pub use crate::retired::Retired;

/// How many threads may participate in one domain at the same time.
///
/// The limit is lower under `loom`: the model explores a switch at every
/// atomic, and walking 64 slots blows up the state space.
#[cfg(not(feature = "loom"))]
pub const MAX_THREADS: usize = 64;
#[cfg(feature = "loom")]
pub const MAX_THREADS: usize = 4;

pub(crate) const INACTIVE: usize = usize::MAX;
