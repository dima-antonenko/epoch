//! A fixed-size chained hash table.
//!
//! The bucket count is chosen at construction and never changes. Replacing
//! the bucket array is a separate problem: the pointer to the array itself
//! would also have to be protected from reclamation. A bucket is a list
//! head. It lives in the table body and never goes to limbo.
//!
//! Inside a bucket is a Michael list sorted by `Ord`. Deletion has two
//! steps. First a CAS sets the low bit of `next`: from that moment the key
//! is logically absent. Then a CAS on the predecessor unlinks the node
//! physically. A search that meets a mark helps unlink the node and, if the
//! unlink succeeds, puts the node into limbo itself. The generation in the
//! link word closes ABA on that CAS.
//!
//! `insert` of an existing key does not publish the new node and destroys it
//! immediately: the node never entered the structure. `get` clones the value
//! while the scheme still holds the node, and it does not hand a reference
//! out. That is why `get` requires `V: Clone`.
//!
//! Implementations exist for hazard pointers and epoch. RCU and deferred
//! cover a whole operation with one section, the same way epoch does, so
//! they were not copied into a third and fourth table.
//!
//! `Drop` of the key and the value can run inside `retire`, still inside the
//! deleting thread's section. That `Drop` must not enter the same table
//! again: a nested hazard-pointer entry writes the same two slots.

mod epoch;
mod hazard;
mod node;

pub use epoch::EpochMap;
pub use hazard::HazardMap;
