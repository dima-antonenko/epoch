//! `push` does not enter the domain. `pop` publishes a ticket before reading
//! the head. `retire` inside the section is allowed: this thread's own label
//! keeps the node alive until the guard's `Drop`.

use std::marker::PhantomData;

use crate::atomic::{Arc, AtomicUsize, Ordering};
use crate::reclaim::{DeferredDomain, Retired};
use crate::tagged;

use super::node::{self, Node};

/// Treiber stack on deferred reclamation by tickets.
pub struct DeferredStack<T> {
    head: AtomicUsize,
    domain: Arc<DeferredDomain>,
    _marker: PhantomData<T>,
}

// `T` moves between threads as a whole. Only `&Self` is shared.
unsafe impl<T: Send> Send for DeferredStack<T> {}
unsafe impl<T: Send> Sync for DeferredStack<T> {}

impl<T> Default for DeferredStack<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> DeferredStack<T> {
    pub fn new() -> Self {
        Self {
            head: AtomicUsize::new(tagged::pack::<Node<T>>(std::ptr::null_mut(), 0, false)),
            domain: DeferredDomain::new(),
            _marker: PhantomData,
        }
    }

    /// Pushes a value onto the top. No deferred section is required.
    pub fn push(&self, value: T) {
        let node = Node::alloc(value);
        loop {
            let head = self.head.load(Ordering::Acquire);
            unsafe { node::store_next(node, tagged::pointer(head)) };
            let new_head = tagged::pack(node, tagged::tag(head).wrapping_add(1), false);
            // `Release` publishes the node fields to whoever loads the head with `Acquire`.
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
        // `enter` publishes the label before the head is read.
        let guard = DeferredDomain::enter(&self.domain);
        loop {
            let head = self.head.load(Ordering::Acquire);
            let ptr = tagged::pointer::<Node<T>>(head);
            if ptr.is_null() {
                return None;
            }
            let next = unsafe { node::load_next(ptr) };
            // The expected word includes the generation read together with the address.
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

impl<T> Drop for DeferredStack<T> {
    fn drop(&mut self) {
        // The live chain is destroyed immediately. Local limbo follows.
        let mut ptr = tagged::pointer::<Node<T>>(self.head.swap(0, Ordering::Acquire));
        while !ptr.is_null() {
            let next = unsafe { node::load_next(ptr) };
            unsafe { node::drop_node::<T>(ptr.cast()) };
            ptr = next;
        }
        self.domain.free_all_for_shutdown();
    }
}
