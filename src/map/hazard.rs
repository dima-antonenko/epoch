//! Michael list on hazard pointers.
//!
//! Slot 1 holds the current node. Slot 0 holds the previous node once the
//! walk steps forward from it. The bucket head is not written into a slot:
//! it lives in the table body.

use std::cmp::Ordering as Cmp;
use std::hash::Hash;
use std::marker::PhantomData;

use crate::atomic::{Arc, AtomicUsize, Ordering};
use crate::reclaim::{HazardDomain, Retired, Shield};
use crate::tagged;

use super::node::{self, Node};

/// Lock-free hash map on hazard pointers.
///
/// Keys are ordered inside a bucket (`Ord`). The bucket index comes from `Hash`.
pub struct HazardMap<K, V> {
    buckets: Box<[AtomicUsize]>,
    domain: Arc<HazardDomain>,
    _marker: PhantomData<(K, V)>,
}

// Several threads compare the key, and the value is cloned under the scheme.
// Both types are therefore `Send + Sync`, unlike the stack and the queue.
unsafe impl<K: Send + Sync, V: Send + Sync> Send for HazardMap<K, V> {}
unsafe impl<K: Send + Sync, V: Send + Sync> Sync for HazardMap<K, V> {}

impl<K, V> HazardMap<K, V>
where
    K: Hash + Ord,
{
    /// `buckets` is the number of chains. There is no resize.
    pub fn new(buckets: usize) -> Self {
        assert!(buckets > 0, "at least one bucket is required");
        let buckets = (0..buckets)
            .map(|_| AtomicUsize::new(tagged::pack::<Node<K, V>>(std::ptr::null_mut(), 0, false)))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            buckets,
            domain: HazardDomain::new(),
            _marker: PhantomData,
        }
    }

    /// Inserts a key. Returns `false` when the key is already present: the
    /// old value stays, the new node never enters the list, and it is
    /// destroyed immediately.
    pub fn insert(&self, key: K, value: V) -> bool {
        let node = Node::alloc(key, value);
        let shield = HazardDomain::enter(&self.domain);
        loop {
            let bucket =
                &self.buckets[node::bucket_index(unsafe { node::key(node) }, self.buckets.len())];
            match find::<K, V>(&shield, bucket, unsafe { node::key(node) }) {
                Spot::Found { .. } => {
                    // The node was never published, so it does not need `retire`.
                    unsafe { node::drop_node::<K, V>(node.cast()) };
                    return false;
                }
                Spot::Missing {
                    prev,
                    curr,
                    curr_raw,
                } => {
                    unsafe {
                        node::store_next(
                            node,
                            tagged::pack(curr, tagged::tag(curr_raw).wrapping_add(1), false),
                        );
                    }
                    let new_raw = tagged::pack(node, tagged::tag(curr_raw).wrapping_add(1), false);
                    // Failure means the link moved. The node stays local, and
                    // the search for an insertion point starts over.
                    if unsafe { node::cas_link::<K, V>(prev, curr_raw, new_raw) }.is_ok() {
                        return true;
                    }
                }
            }
        }
    }

    /// Logically deletes a key. Returns `true` when the key was in the table.
    ///
    /// Another search may finish the physical unlink. The node goes to limbo
    /// only on the thread whose unlink CAS won.
    pub fn remove(&self, key: &K) -> bool {
        let shield = HazardDomain::enter(&self.domain);
        loop {
            let bucket = &self.buckets[node::bucket_index(key, self.buckets.len())];
            match find::<K, V>(&shield, bucket, key) {
                Spot::Missing { .. } => return false,
                Spot::Found {
                    prev,
                    curr,
                    curr_raw,
                } => {
                    let next_raw = unsafe { node::load_next(curr) };
                    if tagged::is_marked(next_raw) {
                        // The key is already marked. Search again: a helper may have unlinked it.
                        continue;
                    }
                    // The mark comes first. Until it is set, a search is still
                    // allowed to return the value.
                    let next_link = unsafe { &raw const (*curr).next };
                    if unsafe {
                        node::cas_link::<K, V>(next_link, next_raw, tagged::with_mark(next_raw))
                    }
                    .is_err()
                    {
                        continue;
                    }
                    let new_raw = tagged::pack(
                        tagged::pointer::<Node<K, V>>(next_raw),
                        tagged::tag(curr_raw).wrapping_add(1),
                        false,
                    );
                    if unsafe { node::cas_link::<K, V>(prev, curr_raw, new_raw) }.is_ok() {
                        shield.clear(0);
                        shield.clear(1);
                        unsafe {
                            shield.retire(Retired::new(curr.cast(), node::drop_node::<K, V>));
                        }
                    }
                    // The mark is already set, even if another thread took the unlink.
                    return true;
                }
            }
        }
    }

    /// A copy of the value. The method does not hand out a reference to the
    /// node: by the time it returns, protection may already be gone.
    pub fn get(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        let shield = HazardDomain::enter(&self.domain);
        loop {
            let bucket = &self.buckets[node::bucket_index(key, self.buckets.len())];
            match find::<K, V>(&shield, bucket, key) {
                Spot::Missing { .. } => return None,
                Spot::Found { curr, .. } => {
                    if tagged::is_marked(unsafe { node::load_next(curr) }) {
                        continue;
                    }
                    let cloned = unsafe { node::clone_value(curr) };
                    // Check again: the key may have been marked between the
                    // clone and the return. That copy is not an observed
                    // value, and the search starts over.
                    if tagged::is_marked(unsafe { node::load_next(curr) }) {
                        continue;
                    }
                    return Some(cloned);
                }
            }
        }
    }
}

impl<K, V> Drop for HazardMap<K, V> {
    fn drop(&mut self) {
        // Nodes that are marked but still hanging in a chain are freed here.
        // Nodes already unlinked sit in limbo and are finished by domain shutdown.
        for bucket in self.buckets.iter() {
            let mut ptr = tagged::pointer::<Node<K, V>>(bucket.swap(0, Ordering::Acquire));
            while !ptr.is_null() {
                let next = tagged::pointer::<Node<K, V>>(unsafe { node::load_next(ptr) });
                unsafe { node::drop_node::<K, V>(ptr.cast()) };
                ptr = next;
            }
        }
        self.domain.free_all_for_shutdown();
    }
}

enum Spot<K, V> {
    /// The key was found. `prev` is the word whose CAS inserts or unlinks the node.
    Found {
        prev: *const AtomicUsize,
        curr: *mut Node<K, V>,
        curr_raw: usize,
    },
    /// The insertion point is in front of `curr`. `curr` is null when the key
    /// is greater than every key in the bucket.
    Missing {
        prev: *const AtomicUsize,
        curr: *mut Node<K, V>,
        curr_raw: usize,
    },
}

fn find<K, V>(shield: &Shield<'_>, head: &AtomicUsize, key: &K) -> Spot<K, V>
where
    K: Ord,
{
    // The search walks a sorted list and helps unlink nodes that are already
    // marked. The bucket head is part of the table, so its address is not
    // written into a slot.
    'restart: loop {
        shield.clear(0);
        let mut prev: *const AtomicUsize = head;
        // The current node occupies slot 1. The bucket head is immortal and takes no slot.
        let mut curr_raw = shield.protect(1, head);
        loop {
            let curr = tagged::pointer::<Node<K, V>>(curr_raw);
            if curr.is_null() {
                return Spot::Missing {
                    prev,
                    curr,
                    curr_raw,
                };
            }
            let next_raw = unsafe { node::load_next(curr) };
            if tagged::is_marked(next_raw) {
                // Unlink a logically deleted node ourselves. A lost CAS means
                // the list changed nearby, and the walk is safer from the start.
                let new_raw = tagged::pack(
                    tagged::pointer::<Node<K, V>>(next_raw),
                    tagged::tag(curr_raw).wrapping_add(1),
                    false,
                );
                match unsafe { node::cas_link::<K, V>(prev, curr_raw, new_raw) } {
                    Ok(_) => {
                        shield.clear(1);
                        unsafe {
                            shield.retire(Retired::new(curr.cast(), node::drop_node::<K, V>));
                        }
                        curr_raw = shield.protect(1, unsafe { &*prev });
                        continue;
                    }
                    Err(_) => continue 'restart,
                }
            }
            if unsafe { (*prev).load(Ordering::Acquire) } != curr_raw {
                // The predecessor no longer points here: the walk is stale.
                continue 'restart;
            }
            match key.cmp(unsafe { node::key(curr) }) {
                Cmp::Equal => {
                    return Spot::Found {
                        prev,
                        curr,
                        curr_raw,
                    };
                }
                Cmp::Less => {
                    return Spot::Missing {
                        prev,
                        curr,
                        curr_raw,
                    };
                }
                Cmp::Greater => {
                    // Slot 0 takes the node that becomes `prev` before slot 1
                    // moves on to the next node.
                    shield.protect_ptr(0, curr);
                    prev = unsafe { &raw const (*curr).next };
                    curr_raw = shield.protect(1, unsafe { &*prev });
                }
            }
        }
    }
}
