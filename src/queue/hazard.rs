//! Hazard pointers hold every dereferenced node explicitly.
//! `enqueue` occupies slot 0 with the tail. `dequeue` occupies slot 0 with
//! the head and slot 1 with the node that holds the value.

use std::marker::PhantomData;
use std::ptr;

use crate::atomic::{Arc, AtomicUsize, Ordering};
use crate::reclaim::{HazardDomain, Retired};
use crate::tagged;

use super::node::{self, Node};

/// Michael–Scott queue on hazard pointers.
pub struct HazardQueue<T> {
    head: AtomicUsize,
    tail: AtomicUsize,
    domain: Arc<HazardDomain>,
    _marker: PhantomData<T>,
}

// `T` moves between threads as a whole. Only `&Self` is shared.
unsafe impl<T: Send> Send for HazardQueue<T> {}
unsafe impl<T: Send> Sync for HazardQueue<T> {}

impl<T> Default for HazardQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> HazardQueue<T> {
    pub fn new() -> Self {
        // The dummy head exists from the start, so `dequeue` on an empty
        // queue frees nothing: there is nothing to unlink yet.
        let dummy = Node::<T>::dummy();
        let raw = tagged::pack(dummy, 0, false);
        Self {
            head: AtomicUsize::new(raw),
            tail: AtomicUsize::new(raw),
            domain: HazardDomain::new(),
            _marker: PhantomData,
        }
    }

    /// Pushes a value onto the tail.
    pub fn enqueue(&self, value: T) {
        let node = Node::alloc(value);
        let shield = HazardDomain::enter(&self.domain);
        loop {
            // The tail is protected: it can be unlinked from the head while we read `next`.
            let tail_raw = shield.protect(0, &self.tail);
            let tail_ptr = tagged::pointer::<Node<T>>(tail_raw);
            let next_raw = unsafe { node::load_next(tail_ptr) };
            if self.tail.load(Ordering::Acquire) != tail_raw {
                continue;
            }
            if tagged::pointer::<Node<T>>(next_raw).is_null() {
                let new_next = tagged::pack(node, tagged::tag(next_raw).wrapping_add(1), false);
                if unsafe { node::cas_next(tail_ptr, next_raw, new_next) }.is_ok() {
                    // The node is already in the list. Losing the tail CAS is safe:
                    // the next `enqueue` or `dequeue` will swing it forward.
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
                // The tail has fallen behind: help swing it forward and try again.
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
    pub fn dequeue(&self) -> Option<T> {
        let shield = HazardDomain::enter(&self.domain);
        loop {
            let head_raw = shield.protect(0, &self.head);
            let head_ptr = tagged::pointer::<Node<T>>(head_raw);
            let next_link = unsafe { &raw const (*head_ptr).next };
            // The second slot holds the node that stores the value until we take it.
            let next_raw = shield.protect(1, unsafe { &*next_link });
            if self.head.load(Ordering::Acquire) != head_raw {
                continue;
            }
            let next_ptr = tagged::pointer::<Node<T>>(next_raw);
            let tail_raw = self.tail.load(Ordering::Acquire);
            if tagged::pointer::<Node<T>>(tail_raw) == head_ptr {
                // Head and tail match. The queue is empty only when nothing follows
                // the head; otherwise the tail has fallen behind and must be swung
                // forward before the next attempt.
                if next_ptr.is_null() {
                    return None;
                }
                let new_tail = tagged::pack(next_ptr, tagged::tag(tail_raw).wrapping_add(1), false);
                let _ = self.tail.compare_exchange(
                    tail_raw,
                    new_tail,
                    Ordering::Release,
                    Ordering::Acquire,
                );
                continue;
            }
            if next_ptr.is_null() {
                // The `next` snapshot is stale: the tail has already moved on,
                // and the head's link was filled in. A null pointer must not be
                // dereferenced.
                continue;
            }
            let new_head = tagged::pack(next_ptr, tagged::tag(head_raw).wrapping_add(1), false);
            if self
                .head
                .compare_exchange(head_raw, new_head, Ordering::Release, Ordering::Acquire)
                .is_ok()
            {
                // Only one thread wins the CAS, and that thread takes the value.
                let value = unsafe { node::take_value(next_ptr) };
                shield.clear(0);
                shield.clear(1);
                unsafe {
                    shield.retire(Retired::new(head_ptr.cast(), node::drop_node::<T>));
                }
                return value;
            }
        }
    }
}

impl<T> Drop for HazardQueue<T> {
    fn drop(&mut self) {
        // Exclusive access: the chain from the head, including the dummy
        // node, is destroyed immediately. Domain limbo is finished separately.
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
