#![cfg(not(feature = "loom"))]

//! Functional checks of the schemes, without loom.
//!
//! A "bomb" is a block whose `Drop` raises a flag. While the flag is clear,
//! the block has not been freed yet. The tests hold protection on one
//! thread and check that another thread's `retire` does not destroy it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use epoch::reclaim::{DeferredDomain, EpochDomain, HazardDomain, RcuDomain, Retired};
use epoch::tagged;

unsafe fn free_flag(ptr: *mut u8) {
    let flag = unsafe { Box::from_raw(ptr.cast::<Arc<AtomicBool>>()) };
    flag.store(true, Ordering::SeqCst);
}

fn bomb() -> (usize, Arc<AtomicBool>) {
    let flag = Arc::new(AtomicBool::new(false));
    let ptr = Box::into_raw(Box::new(Arc::clone(&flag)));
    (ptr as usize, flag)
}

fn retire_bomb(addr: usize) -> Retired {
    unsafe { Retired::new(addr as *mut u8, free_flag) }
}

#[test]
fn hazard_does_not_free_a_protected_pointer() {
    let domain = HazardDomain::new();
    let (addr, flag) = bomb();
    let slot = AtomicUsize::new(tagged::pack(addr as *mut u8, 1, false));
    let shield = HazardDomain::enter(&domain);
    assert_eq!(
        shield.protect(0, &slot),
        tagged::pack(addr as *mut u8, 1, false)
    );
    unsafe { shield.retire(retire_bomb(addr)) };
    domain.try_reclaim();
    assert!(!flag.load(Ordering::SeqCst));
    shield.clear(0);
    domain.try_reclaim();
    assert!(flag.load(Ordering::SeqCst));
}

#[test]
fn epoch_does_not_free_while_pinned() {
    let domain = EpochDomain::new();
    let (addr, flag) = bomb();
    let guard = EpochDomain::pin(&domain);
    unsafe { guard.retire(retire_bomb(addr)) };
    domain.try_reclaim();
    assert!(
        !flag.load(Ordering::SeqCst),
        "node was destroyed while the pin was still held"
    );
    drop(guard);
    assert!(
        flag.load(Ordering::SeqCst),
        "node was not destroyed after leaving the pin"
    );
}

#[test]
fn deferred_does_not_free_inside_section() {
    let domain = DeferredDomain::new();
    let (addr, flag) = bomb();
    let guard = DeferredDomain::enter(&domain);
    unsafe { guard.retire(retire_bomb(addr)) };
    domain.try_reclaim();
    assert!(!flag.load(Ordering::SeqCst));
    drop(guard);
    assert!(flag.load(Ordering::SeqCst));
}

#[test]
fn rcu_waits_for_the_reader_cohort() {
    let domain = RcuDomain::new();
    let guard = RcuDomain::lock(&domain);
    let (addr, flag) = bomb();
    let domain2 = Arc::clone(&domain);
    let started = Arc::new(AtomicBool::new(false));
    let started2 = Arc::clone(&started);
    let worker = thread::spawn(move || {
        started2.store(true, Ordering::SeqCst);
        unsafe { domain2.retire(retire_bomb(addr)) };
    });
    while !started.load(Ordering::SeqCst) {
        thread::yield_now();
    }
    for _ in 0..2_000 {
        thread::yield_now();
        assert!(
            !flag.load(Ordering::SeqCst),
            "RCU destroyed the node while a reader was still in its section"
        );
    }
    drop(guard);
    worker.join().unwrap();
    assert!(flag.load(Ordering::SeqCst));
}

#[test]
fn hazard_scan_sees_another_thread() {
    let domain = HazardDomain::new();
    let (addr, flag) = bomb();
    let slot = Arc::new(AtomicUsize::new(tagged::pack(addr as *mut u8, 1, false)));
    let shield = HazardDomain::enter(&domain);
    shield.protect(0, &slot);
    let domain2 = Arc::clone(&domain);
    let worker = thread::spawn(move || {
        let local = HazardDomain::enter(&domain2);
        unsafe { local.retire(retire_bomb(addr)) };
        domain2.try_reclaim();
    });
    worker.join().unwrap();
    assert!(!flag.load(Ordering::SeqCst));
    shield.clear(0);
    domain.try_reclaim();
    assert!(flag.load(Ordering::SeqCst));
}

#[test]
fn epoch_pin_blocks_another_thread() {
    let domain = EpochDomain::new();
    let guard = EpochDomain::pin(&domain);
    let (addr, flag) = bomb();
    let domain2 = Arc::clone(&domain);
    let worker = thread::spawn(move || {
        let local = EpochDomain::pin(&domain2);
        unsafe { local.retire(retire_bomb(addr)) };
        drop(local);
    });
    worker.join().unwrap();
    assert!(!flag.load(Ordering::SeqCst));
    drop(guard);
    assert!(flag.load(Ordering::SeqCst));
}

#[test]
fn deferred_clock_blocks_another_thread() {
    let domain = DeferredDomain::new();
    let guard = DeferredDomain::enter(&domain);
    let (addr, flag) = bomb();
    let domain2 = Arc::clone(&domain);
    let worker = thread::spawn(move || {
        let local = DeferredDomain::enter(&domain2);
        unsafe { local.retire(retire_bomb(addr)) };
        drop(local);
        domain2.try_reclaim();
    });
    worker.join().unwrap();
    assert!(!flag.load(Ordering::SeqCst));
    drop(guard);
    assert!(flag.load(Ordering::SeqCst));
}
