//! Michael–Scott queue.
//!
//! In an empty queue `head` and `tail` both point at one dummy node that
//! holds no value. Data lives in the nodes that follow the head. `enqueue`
//! first links the new node with a CAS on `tail.next`, then swings `tail`
//! onto that node. Another thread may swing the tail: if the tail CAS loses,
//! the node is already in the list, and the next attempt sees a non-null
//! `next`.
//!
//! `dequeue` unlinks the dummy head. The value is taken from the next node,
//! and that node becomes the new head. The old head goes to limbo: a neighbor
//! may still be reading its `next`. The generation sits in `head`/`tail` and
//! in `next`, because the link keeps being changed by CAS after publication.
//! On the stack the link is immutable after publication, so the generation
//! lives only in the head.
//!
//! Protection by scheme:
//!
//! - hazard pointers hold the tail in `enqueue`, and in `dequeue` they hold
//!   the head and the node that stores the value at the same time (two slots);
//! - epoch and deferred hold a section for the whole operation;
//! - RCU closes the read-side section before `retire` in `dequeue`.
//!   `enqueue` defers nothing.

mod deferred;
mod epoch;
mod hazard;
mod node;
mod rcu;

pub use deferred::DeferredQueue;
pub use epoch::EpochQueue;
pub use hazard::HazardQueue;
pub use rcu::RcuQueue;
