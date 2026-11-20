//! The same steps as the hazard queue. A read-side section replaces the
//! slots. In `dequeue` the section ends before `retire`, otherwise
//! synchronization would wait on this thread's own counter. `enqueue`
//! defers no nodes.

use std::marker::PhantomData;
use std::ptr;

use crate::atomic::{Arc, AtomicUsize, Ordering};
use crate::reclaim::{RcuDomain, Retired};
use crate::tagged;

use super::node::{self, Node};

/// Michael–Scott queue on userspace RCU.
pub struct RcuQueue<T> {
    head: AtomicUsize,
    tail: AtomicUsize,
    domain: Arc<RcuDomain>,
    _marker: PhantomData<T>,
}

// `T` moves between threads as a whole. Only `&Self` is shared.
unsafe impl<T: Send> Send for RcuQueue<T> {}
unsafe impl<T: Send> Sync for RcuQueue<T> {}

impl<T> Default for RcuQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> RcuQueue<T> {
    pub fn new() -> Self {
        let dummy = Node::<T>::dummy();
        let raw = tagged::pack(dummy, 0, false);
        Self {
            head: AtomicUsize::new(raw),
            tail: AtomicUsize::new(raw),
            domain: RcuDomain::new(),
            _marker: PhantomData,
        }
    }

    /// Pushes a value onto the tail.
    pub fn enqueue(&self, value: T) {
        let node = Node::alloc(value);
        // Reading `tail.next` dereferences a shared node, so a section is required.
        let _guard = RcuDomain::lock(&self.domain);
        loop {
            let tail_raw = self.tail.load(Ordering::Acquire);
            let tail_ptr = tagged::pointer::<Node<T>>(tail_raw);
            let next_raw = unsafe { node::load_next(tail_ptr) };
            if self.tail.load(Ordering::Acquire) != tail_raw {
                continue;
            }
            if tagged::pointer::<Node<T>>(next_raw).is_null() {
                let new_next = tagged::pack(node, tagged::tag(next_raw).wrapping_add(1), false);
                if unsafe { node::cas_next(tail_ptr, next_raw, new_next) }.is_ok() {
                    // The link is already published. The tail can be swung later.
                    let new_tail = tagged::pack(node, tagged::tag(tail_raw).wrapping_add(1), false);
                    let _ = self.tail.compare_exchange(
                        tail_raw,
                        new_tail,
                        Ordering::Release,
                        Ordering::Acquire,
                    );
                    return;
                }
            } else {
                // The tail does not point at the last node.
                let new_tail = tagged::pack(
                    tagged::pointer::<Node<T>>(next_raw),
                    tagged::tag(tail_raw).wrapping_add(1),
                    false,
                );
                let _ = self.tail.compare_exchange(
                    tail_raw,
                    new_tail,
                    Ordering::Release,
                    Ordering::Acquire,
                );
            }
        }
    }

    /// Takes a value from the head. `None` when the queue is empty.
    ///
    /// The guard lives in its own block and is dropped before `retire`.
    pub fn dequeue(&self) -> Option<T> {
        let retired = {
            let _guard = RcuDomain::lock(&self.domain);
            loop {
                let head_raw = self.head.load(Ordering::Acquire);
                let head_ptr = tagged::pointer::<Node<T>>(head_raw);
                let next_raw = unsafe { node::load_next(head_ptr) };
                if self.head.load(Ordering::Acquire) != head_raw {
                    continue;
                }
                let next_ptr = tagged::pointer::<Node<T>>(next_raw);
                let tail_raw = self.tail.load(Ordering::Acquire);
                if tagged::pointer::<Node<T>>(tail_raw) == head_ptr {
                    if next_ptr.is_null() {
                        break None;
                    }
                    // A node already follows the head, and the tail still looks at the head.
                    let new_tail =
                        tagged::pack(next_ptr, tagged::tag(tail_raw).wrapping_add(1), false);
                    let _ = self.tail.compare_exchange(
                        tail_raw,
                        new_tail,
                        Ordering::Release,
                        Ordering::Acquire,
                    );
                    continue;
                }
                if next_ptr.is_null() {
                    // The `next` snapshot is stale relative to the tail.
                    continue;
                }
                let new_head = tagged::pack(next_ptr, tagged::tag(head_raw).wrapping_add(1), false);
                if self
                    .head
                    .compare_exchange(head_raw, new_head, Ordering::Release, Ordering::Acquire)
                    .is_ok()
                {
                    let value = unsafe { node::take_value(next_ptr) };
                    // The pair that leaves the section: the value, and the dummy head to retire.
                    break Some((value, head_ptr));
                }
            }
        };
        match retired {
            Some((value, head_ptr)) => {
                unsafe {
                    self.domain
                        .retire(Retired::new(head_ptr.cast(), node::drop_node::<T>));
                }
                value
            }
            None => None,
        }
    }
}

impl<T> Drop for RcuQueue<T> {
    fn drop(&mut self) {
        // The chain is destroyed immediately. Cohort queues are drained by domain shutdown.
        let mut ptr = tagged::pointer::<Node<T>>(self.head.load(Ordering::Acquire));
        while !ptr.is_null() {
            let next = tagged::pointer::<Node<T>>(unsafe { node::load_next(ptr) });
            unsafe { node::drop_node::<T>(ptr.cast()) };
            ptr = next;
        }
        self.head.store(
            tagged::pack::<Node<T>>(ptr::null_mut(), 0, false),
            Ordering::Relaxed,
        );
        self.tail.store(
            tagged::pack::<Node<T>>(ptr::null_mut(), 0, false),
            Ordering::Relaxed,
        );
        self.domain.free_all_for_shutdown();
    }
}
