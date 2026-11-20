//! `push` does not enter the domain. `pop` holds the top in slot 0, clears
//! the slot, and only then puts the node into limbo: the thread's own slot
//! would otherwise keep the node forever.

use std::marker::PhantomData;

use crate::atomic::{Arc, AtomicUsize, Ordering};
use crate::reclaim::{HazardDomain, Retired};
use crate::tagged;

use super::node::{self, Node};

/// Treiber stack on hazard pointers.
pub struct HazardStack<T> {
    head: AtomicUsize,
    domain: Arc<HazardDomain>,
    _marker: PhantomData<T>,
}

// Same rule as `Mutex`: ownership of `T` moves between threads, and only
// `&Self` is shared.
unsafe impl<T: Send> Send for HazardStack<T> {}
unsafe impl<T: Send> Sync for HazardStack<T> {}

impl<T> Default for HazardStack<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> HazardStack<T> {
    pub fn new() -> Self {
        Self {
            head: AtomicUsize::new(tagged::pack::<Node<T>>(std::ptr::null_mut(), 0, false)),
            domain: HazardDomain::new(),
            _marker: PhantomData,
        }
    }

    /// Pushes a value onto the top.
    pub fn push(&self, value: T) {
        let node = Node::alloc(value);
        loop {
            let head = self.head.load(Ordering::Acquire);
            unsafe { node::store_next(node, tagged::pointer(head)) };
            // A new generation makes a later insertion of the same address a different word.
            let new_head = tagged::pack(node, tagged::tag(head).wrapping_add(1), false);
            // `Release` publishes `value` and `next` together with the new root.
            if self
                .head
                .compare_exchange(head, new_head, Ordering::Release, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Pops the top. `None` when the stack is empty.
    pub fn pop(&self) -> Option<T> {
        let shield = HazardDomain::enter(&self.domain);
        loop {
            let head = shield.protect(0, &self.head);
            let ptr = tagged::pointer::<Node<T>>(head);
            if ptr.is_null() {
                return None;
            }
            // The top address is already in a hazard slot, so `next` may be read.
            let next = unsafe { node::load_next(ptr) };
            // The CAS compares the whole word, including the generation read with the address.
            let new_head = tagged::pack(next, tagged::tag(head).wrapping_add(1), false);
            if self
                .head
                .compare_exchange(head, new_head, Ordering::Release, Ordering::Acquire)
                .is_ok()
            {
                let value = unsafe { node::take_value(ptr) };
                // Our own slot must not keep the node we just unlinked.
                shield.clear(0);
                unsafe {
                    shield.retire(Retired::new(ptr.cast(), node::drop_node::<T>));
                }
                return value;
            }
        }
    }
}

impl<T> Drop for HazardStack<T> {
    fn drop(&mut self) {
        // Exclusive access: the live chain can be destroyed immediately.
        // Deferred nodes of this thread, and orphans, live in the domain separately.
        let mut ptr = tagged::pointer::<Node<T>>(self.head.swap(0, Ordering::Acquire));
        while !ptr.is_null() {
            let next = unsafe { node::load_next(ptr) };
            unsafe { node::drop_node::<T>(ptr.cast()) };
            ptr = next;
        }
        self.domain.free_all_for_shutdown();
    }
}
