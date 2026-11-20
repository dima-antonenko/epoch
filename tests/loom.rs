#![cfg(feature = "loom")]

//! Short loom models.
//!
//! The search is bounded: two threads, `preemption_bound = 2`, at most
//! 8000 branches. It checks the interleavings of a small scenario. The
//! model does not cover every path of the four schemes and does not replace
//! the functional tests.
//!
//! With the `loom` feature a domain has 4 slots. In this build the crate's
//! atomics are the model's atomics, and the TLS destructor does not touch
//! them.

use epoch::reclaim::{EpochDomain, Retired};
use epoch::{DeferredStack, EpochQueue, EpochStack, HazardStack, RcuStack};
use std::sync::Arc;

use loom::sync::atomic::{AtomicBool, Ordering};
use loom::thread;

fn explore(f: impl Fn() + Send + Sync + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(2);
    builder.max_branches = 8_000;
    builder.check(f);
}

fn stack_keeps_both<S>(push: fn(&S, i32), pop: fn(&S) -> Option<i32>, new: fn() -> S)
where
    S: Send + Sync + 'static,
{
    explore(move || {
        let stack = Arc::new(new());
        let other = Arc::clone(&stack);
        let worker = thread::spawn(move || {
            push(&other, 1);
            pop(&other)
        });
        push(&stack, 2);
        let mut got = Vec::new();
        if let Some(value) = pop(&stack) {
            got.push(value);
        }
        if let Some(value) = worker.join().unwrap() {
            got.push(value);
        }
        while let Some(value) = pop(&stack) {
            got.push(value);
        }
        got.sort_unstable();
        assert_eq!(got, vec![1, 2]);
    });
}

#[test]
fn hazard_stack_keeps_elements() {
    stack_keeps_both(HazardStack::push, HazardStack::pop, HazardStack::new);
}

#[test]
fn epoch_stack_keeps_elements() {
    stack_keeps_both(EpochStack::push, EpochStack::pop, EpochStack::new);
}

#[test]
fn rcu_stack_keeps_elements() {
    stack_keeps_both(RcuStack::push, RcuStack::pop, RcuStack::new);
}

#[test]
fn deferred_stack_keeps_elements() {
    stack_keeps_both(DeferredStack::push, DeferredStack::pop, DeferredStack::new);
}

#[test]
fn epoch_queue_keeps_order_for_one_producer() {
    explore(|| {
        let queue = Arc::new(EpochQueue::new());
        let other = Arc::clone(&queue);
        let worker = thread::spawn(move || {
            let mut got = Vec::new();
            if let Some(value) = other.dequeue() {
                got.push(value);
            }
            if let Some(value) = other.dequeue() {
                got.push(value);
            }
            got
        });
        queue.enqueue(1);
        queue.enqueue(2);
        let mut got = worker.join().unwrap();
        while let Some(value) = queue.dequeue() {
            got.push(value);
        }
        assert_eq!(got, vec![1, 2]);
    });
}

unsafe fn free_flag(ptr: *mut u8) {
    let flag = unsafe { Box::from_raw(ptr.cast::<Arc<AtomicBool>>()) };
    flag.store(true, Ordering::Release);
}

#[test]
fn epoch_does_not_drop_while_the_other_thread_is_pinned() {
    explore(|| {
        let domain = EpochDomain::new();
        let flag = Arc::new(AtomicBool::new(false));
        let boxed = Box::into_raw(Box::new(Arc::clone(&flag)));
        let phase = Arc::new(loom::sync::atomic::AtomicUsize::new(0));
        let domain2 = Arc::clone(&domain);
        let phase2 = Arc::clone(&phase);
        let flag2 = Arc::clone(&flag);
        let worker = thread::spawn(move || {
            let guard = EpochDomain::pin(&domain2);
            phase2.store(1, Ordering::Release);
            while phase2.load(Ordering::Acquire) < 2 {
                thread::yield_now();
            }
            assert!(!flag2.load(Ordering::Acquire));
            drop(guard);
            phase2.store(3, Ordering::Release);
        });
        while phase.load(Ordering::Acquire) < 1 {
            thread::yield_now();
        }
        let guard = EpochDomain::pin(&domain);
        unsafe { guard.retire(Retired::new(boxed.cast(), free_flag)) };
        drop(guard);
        phase.store(2, Ordering::Release);
        while phase.load(Ordering::Acquire) < 3 {
            thread::yield_now();
        }
        domain.try_reclaim();
        assert!(flag.load(Ordering::Acquire));
        worker.join().unwrap();
    });
}
