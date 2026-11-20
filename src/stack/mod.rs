//! Treiber stack on each reclamation scheme.
//!
//! The head is one packed word: the address of the top and a generation.
//! `push` writes `next` into a node nobody else can see yet and publishes it
//! with a `Release` CAS of the head. It does not dereference a foreign node,
//! so it does not enter a scheme.
//!
//! `pop` enters a scheme, reads the top's `next`, and CASes the head onto
//! that address with a new generation. The generation closes ABA. Without
//! it a thread can read `next`, fall behind, and watch the node get removed
//! and inserted again with a different chain: a CAS on the bare address
//! would then unlink the wrong chain.
//!
//! Which guard `pop` uses:
//!
//! - hazard pointers hold the top address in a slot while `next` is read;
//! - epoch and deferred hold a section for the whole loop, including `retire`;
//! - RCU closes the read-side section before `retire`, otherwise
//!   `synchronize` would wait on this thread's own counter.
//!
//! `Drop` of the stack walks the live chain without a scheme: `&mut self`
//! means there are no other accesses. Nodes already in limbo are taken by
//! domain shutdown.

mod deferred;
mod epoch;
mod hazard;
mod node;
mod rcu;

pub use deferred::DeferredStack;
pub use epoch::EpochStack;
pub use hazard::HazardStack;
pub use rcu::RcuStack;
