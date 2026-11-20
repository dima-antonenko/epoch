//! A node of the Treiber stack.
//!
//! `next` is written once, while the node is still unpublished, and is only
//! read after that. The generation lives in the head word, not in the link:
//! the link does not change after publication, so there is no CAS on it.
//! The stack does not use the low bit of the packed word.

use std::ptr;

use crate::atomic::{AtomicPtr, Ordering};

/// The canary catches a repeated `free` in tests and under Miri. It checks
/// an invariant. It is not a defense against memory corruption.
pub(crate) const CANARY: usize = 0xE4C0_FFEE_E4C0_FFEE;

#[repr(C)]
pub(crate) struct Node<T> {
    canary: usize,
    value: Option<T>,
    next: AtomicPtr<Node<T>>,
}

impl<T> Node<T> {
    pub(crate) fn alloc(value: T) -> *mut Self {
        Box::into_raw(Box::new(Self {
            canary: CANARY,
            value: Some(value),
            next: AtomicPtr::new(ptr::null_mut()),
        }))
    }
}

/// Destroys a node. The inner `Option` may already be empty: `pop` took the value.
///
/// # Safety
///
/// `ptr` is a node allocated by [`Node::alloc`], and it has not been freed yet.
pub(crate) unsafe fn drop_node<T>(ptr: *mut u8) {
    let node = ptr.cast::<Node<T>>();
    unsafe {
        assert_eq!((*node).canary, CANARY, "double free of a stack node");
        (*node).canary = 0;
        drop(Box::from_raw(node));
    }
}

pub(crate) unsafe fn load_next<T>(ptr: *mut Node<T>) -> *mut Node<T> {
    // `Acquire` pairs with the `Release` CAS of the head: fields written
    // before publication are visible to this load.
    let next = unsafe { &raw const (*ptr).next };
    unsafe { (*next).load(Ordering::Acquire) }
}

pub(crate) unsafe fn store_next<T>(ptr: *mut Node<T>, next: *mut Node<T>) {
    // `Relaxed` is enough: the node is still private. A later successful
    // `Release` CAS of the head publishes its fields.
    let slot = unsafe { &raw const (*ptr).next };
    unsafe { (*slot).store(next, Ordering::Relaxed) };
}

pub(crate) unsafe fn take_value<T>(ptr: *mut Node<T>) -> Option<T> {
    let value = unsafe { &raw mut (*ptr).value };
    unsafe { (*value).take() }
}
