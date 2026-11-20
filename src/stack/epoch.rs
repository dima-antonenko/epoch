//! `push` does not enter the domain. `pop` holds a pin for the whole loop,
//! including `retire`: the published epoch cannot reach `e + 2` while the
//! guard is alive.

use std::marker::PhantomData;

use crate::atomic::{Arc, AtomicUsize, Ordering};
use crate::reclaim::{EpochDomain, Retired};
use crate::tagged;

use super::node::{self, Node};

/// Treiber stack on epoch-based reclamation.
pub struct EpochStack<T> {
    head: AtomicUsize,
    domain: Arc<EpochDomain>,
    _marker: PhantomData<T>,
}

// `T` moves between threads as a whole. Only `&Self` is shared.
unsafe impl<T: Send> Send for EpochStack<T> {}
unsafe impl<T: Send> Sync for EpochStack<T> {}

impl<T> Default for EpochStack<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> EpochStack<T> {
    pub fn new() -> Self {
        Self {
            head: AtomicUsize::new(tagged::pack::<Node<T>>(std::ptr::null_mut(), 0, false)),
            domain: EpochDomain::new(),
            _marker: PhantomData,
        }
    }

    /// Pushes a value onto the top. No pin: no foreign node is read.
    pub fn push(&self, value: T) {
        let node = Node::alloc(value);
        loop {
            let head = self.head.load(Ordering::Acquire);
            unsafe { node::store_next(node, tagged::pointer(head)) };
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
        // The pin covers the whole operation: a node we read is not destroyed
        // before we leave.
        let guard = EpochDomain::pin(&self.domain);
        loop {
            let head = self.head.load(Ordering::Acquire);
            let ptr = tagged::pointer::<Node<T>>(head);
            if ptr.is_null() {
                return None;
            }
            let next = unsafe { node::load_next(ptr) };
            let new_head = tagged::pack(next, tagged::tag(head).wrapping_add(1), false);
            if self
                .head
                .compare_exchange(head, new_head, Ordering::Release, Ordering::Acquire)
                .is_ok()
            {
                let value = unsafe { node::take_value(ptr) };
                unsafe {
                    guard.retire(Retired::new(ptr.cast(), node::drop_node::<T>));
                }
                return value;
            }
        }
    }
}

impl<T> Drop for EpochStack<T> {
    fn drop(&mut self) {
        // The live chain is destroyed immediately. The domain's limbo follows.
        let mut ptr = tagged::pointer::<Node<T>>(self.head.swap(0, Ordering::Acquire));
        while !ptr.is_null() {
            let next = unsafe { node::load_next(ptr) };
            unsafe { node::drop_node::<T>(ptr.cast()) };
            ptr = next;
        }
        self.domain.free_all_for_shutdown();
    }
}
