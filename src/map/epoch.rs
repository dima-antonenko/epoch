//! The same Michael list as the hazard table.
//! A pin over the whole operation replaces per-node slots: reachable nodes
//! stay alive until the guard is dropped.

use std::cmp::Ordering as Cmp;
use std::hash::Hash;
use std::marker::PhantomData;

use crate::atomic::{Arc, AtomicUsize, Ordering};
use crate::reclaim::{EpochDomain, EpochGuard, Retired};
use crate::tagged;

use super::node::{self, Node};

/// Lock-free hash map on epoch-based reclamation.
///
/// Keys are ordered inside a bucket (`Ord`). The bucket index comes from `Hash`.
pub struct EpochMap<K, V> {
    buckets: Box<[AtomicUsize]>,
    domain: Arc<EpochDomain>,
    _marker: PhantomData<(K, V)>,
}

// Several threads compare the key, and the value is cloned under the pin.
// Both types are therefore `Send + Sync`.
unsafe impl<K: Send + Sync, V: Send + Sync> Send for EpochMap<K, V> {}
unsafe impl<K: Send + Sync, V: Send + Sync> Sync for EpochMap<K, V> {}

impl<K, V> EpochMap<K, V>
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
            domain: EpochDomain::new(),
            _marker: PhantomData,
        }
    }

    /// Inserts a key. Returns `false` when the key is already present: the
    /// new node is not published and is destroyed immediately.
    pub fn insert(&self, key: K, value: V) -> bool {
        let node = Node::alloc(key, value);
        let guard = EpochDomain::pin(&self.domain);
        loop {
            let bucket =
                &self.buckets[node::bucket_index(unsafe { node::key(node) }, self.buckets.len())];
            match find::<K, V>(bucket, unsafe { node::key(node) }, &guard) {
                Spot::Found { .. } => {
                    // The node never entered the list, so there is nowhere to defer it.
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
                    // `Release` publishes the new node's key, value, and `next`.
                    if unsafe { node::cas_link::<K, V>(prev, curr_raw, new_raw) }.is_ok() {
                        return true;
                    }
                }
            }
        }
    }

    /// Logically deletes a key. Returns `true` when the key was in the table.
    /// The next search may finish the unlink and the `retire` if this CAS loses.
    pub fn remove(&self, key: &K) -> bool {
        let guard = EpochDomain::pin(&self.domain);
        loop {
            let bucket = &self.buckets[node::bucket_index(key, self.buckets.len())];
            match find::<K, V>(bucket, key, &guard) {
                Spot::Missing { .. } => return false,
                Spot::Found {
                    prev,
                    curr,
                    curr_raw,
                } => {
                    let next_raw = unsafe { node::load_next(curr) };
                    if tagged::is_marked(next_raw) {
                        // The node is already marked, so the walk to this point is stale.
                        continue;
                    }
                    // Logical deletion: the mark in `next` is visible before the physical unlink.
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
                        unsafe {
                            guard.retire(Retired::new(curr.cast(), node::drop_node::<K, V>));
                        }
                    }
                    // The key is already invisible to search: the mark stands whether or not the unlink won.
                    return true;
                }
            }
        }
    }

    /// A copy of the value, taken under the pin. No reference to node memory leaves the method.
    pub fn get(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        let guard = EpochDomain::pin(&self.domain);
        loop {
            let bucket = &self.buckets[node::bucket_index(key, self.buckets.len())];
            match find::<K, V>(bucket, key, &guard) {
                Spot::Missing { .. } => return None,
                Spot::Found { curr, .. } => {
                    if tagged::is_marked(unsafe { node::load_next(curr) }) {
                        continue;
                    }
                    // The clone runs under the pin: another thread's `retire`
                    // does not destroy `V` until this section ends.
                    let cloned = unsafe { node::clone_value(curr) };
                    if tagged::is_marked(unsafe { node::load_next(curr) }) {
                        continue;
                    }
                    return Some(cloned);
                }
            }
        }
    }
}

impl<K, V> Drop for EpochMap<K, V> {
    fn drop(&mut self) {
        // Marked nodes that have not been unlinked yet still hang in the chains and fall here.
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
    Found {
        prev: *const AtomicUsize,
        curr: *mut Node<K, V>,
        curr_raw: usize,
    },
    Missing {
        prev: *const AtomicUsize,
        curr: *mut Node<K, V>,
        curr_raw: usize,
    },
}

fn find<K, V>(head: &AtomicUsize, key: &K, guard: &EpochGuard<'_>) -> Spot<K, V>
where
    K: Ord,
{
    // The pin already keeps every reachable node alive, so separate slots
    // are unnecessary. A marked node is unlinked here and retired immediately.
    'restart: loop {
        let mut prev: *const AtomicUsize = head;
        let mut curr_raw = unsafe { (*prev).load(Ordering::Acquire) };
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
                let new_raw = tagged::pack(
                    tagged::pointer::<Node<K, V>>(next_raw),
                    tagged::tag(curr_raw).wrapping_add(1),
                    false,
                );
                match unsafe { node::cas_link::<K, V>(prev, curr_raw, new_raw) } {
                    Ok(_) => {
                        unsafe {
                            guard.retire(Retired::new(curr.cast(), node::drop_node::<K, V>));
                        }
                        curr_raw = unsafe { (*prev).load(Ordering::Acquire) };
                        continue;
                    }
                    Err(_) => continue 'restart,
                }
            }
            if unsafe { (*prev).load(Ordering::Acquire) } != curr_raw {
                // The predecessor changed its link while we were reading the node.
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
                    prev = unsafe { &raw const (*curr).next };
                    curr_raw = next_raw;
                }
            }
        }
    }
}
