//! The same steps as the hazard queue. A pin replaces the two slots: any
//! node reachable in this section stays alive until the section ends.
//! `enqueue` defers nothing. `dequeue` retires the old head without leaving
//! the pin.

use std::marker::PhantomData;
use std::ptr;

use crate::atomic::{Arc, AtomicUsize, Ordering};
use crate::reclaim::{EpochDomain, Retired};
use crate::tagged;

use super::node::{self, Node};

/// Michael–Scott queue on epoch-based reclamation.
pub struct EpochQueue<T> {
    head: AtomicUsize,
    tail: AtomicUsize,
    domain: Arc<EpochDomain>,
    _marker: PhantomData<T>,
}

// `T` moves between threads as a whole. Only `&Self` is shared.
unsafe impl<T: Send> Send for EpochQueue<T> {}
unsafe impl<T: Send> Sync for EpochQueue<T> {}

impl<T> Default for EpochQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> EpochQueue<T> {
    pub fn new() -> Self {
        // The dummy node is required immediately: an empty queue is a head with no successor.
        let dummy = Node::<T>::dummy();
        let raw = tagged::pack(dummy, 0, false);
        Self {
            head: AtomicUsize::new(raw),
            tail: AtomicUsize::new(raw),
            domain: EpochDomain::new(),
            _marker: PhantomData,
        }
    }

    /// Pushes a value onto the tail.
    pub fn enqueue(&self, value: T) {
        let node = Node::alloc(value);
        // The tail is dereferenced, so the pin covers the whole loop.
        let _guard = EpochDomain::pin(&self.domain);
        loop {
            let tail_raw = self.tail.load(Ordering::Acquire);
            let tail_ptr = tagged::pointer::<Node<T>>(tail_raw);
            let next_raw = unsafe { node::load_next(tail_ptr) };
            if self.tail.load(Ordering::Acquire) != tail_raw {
                // The tail changed between the two loads.
                continue;
            }
            if tagged::pointer::<Node<T>>(next_raw).is_null() {
                let new_next = tagged::pack(node, tagged::tag(next_raw).wrapping_add(1), false);
                if unsafe { node::cas_next(tail_ptr, next_raw, new_next) }.is_ok() {
                    // The node is already visible. Swinging the tail may lose.
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
                // The tail has fallen behind the last node: swing it and look for the end again.
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
        let guard = EpochDomain::pin(&self.domain);
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
                    return None;
                }
                // The tail has fallen behind, even though a node already follows the head.
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
                // The dummy head goes to limbo. The value was already taken from the next node.
                unsafe {
                    guard.retire(Retired::new(head_ptr.cast(), node::drop_node::<T>));
                }
                return value;
            }
        }
    }
}

impl<T> Drop for EpochQueue<T> {
    fn drop(&mut self) {
        // The chain from the head is destroyed immediately. Domain limbo follows.
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
