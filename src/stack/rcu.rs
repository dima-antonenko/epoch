//! `push` does not enter the domain. The read-side section of `pop` ends
//! before `retire`: RCU synchronization must not count this thread's own
//! reader counter.

use std::marker::PhantomData;

use crate::atomic::{Arc, AtomicUsize, Ordering};
use crate::reclaim::{RcuDomain, Retired};
use crate::tagged;

use super::node::{self, Node};

/// Treiber stack on userspace RCU.
pub struct RcuStack<T> {
    head: AtomicUsize,
    domain: Arc<RcuDomain>,
    _marker: PhantomData<T>,
}

// `T` moves between threads as a whole. Only `&Self` is shared.
unsafe impl<T: Send> Send for RcuStack<T> {}
unsafe impl<T: Send> Sync for RcuStack<T> {}

impl<T> Default for RcuStack<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> RcuStack<T> {
    pub fn new() -> Self {
        Self {
            head: AtomicUsize::new(tagged::pack::<Node<T>>(std::ptr::null_mut(), 0, false)),
            domain: RcuDomain::new(),
            _marker: PhantomData,
        }
    }

    /// Pushes a value onto the top. No read-side section is required.
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
        // The read-side section ends before `retire`: synchronize must not wait for us.
        let retired = {
            let _guard = RcuDomain::lock(&self.domain);
            loop {
                let head = self.head.load(Ordering::Acquire);
                let ptr = tagged::pointer::<Node<T>>(head);
                if ptr.is_null() {
                    break None;
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
                    break Some((value, ptr));
                }
            }
        };
        match retired {
            Some((value, ptr)) => {
                unsafe {
                    self.domain
                        .retire(Retired::new(ptr.cast(), node::drop_node::<T>));
                }
                value
            }
            None => None,
        }
    }
}

impl<T> Drop for RcuStack<T> {
    fn drop(&mut self) {
        // The live chain is destroyed immediately. The cohort queues follow.
        let mut ptr = tagged::pointer::<Node<T>>(self.head.swap(0, Ordering::Acquire));
        while !ptr.is_null() {
            let next = unsafe { node::load_next(ptr) };
            unsafe { node::drop_node::<T>(ptr.cast()) };
            ptr = next;
        }
        self.domain.free_all_for_shutdown();
    }
}
