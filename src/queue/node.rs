//! A node of the Michael–Scott queue.
//!
//! The value lives in the node. The head always points at a dummy node with
//! no value. `next` is a packed word, not a bare pointer: after publication
//! an `enqueue` CAS changes it, and the generation has something to tell
//! apart. The queue does not use the low deletion-mark bit.

use crate::atomic::{AtomicUsize, Ordering};
use crate::tagged;

/// The canary catches a repeated `free`. It checks an invariant. It is not a barrier.
pub(crate) const CANARY: usize = 0xC0FF_EE00_C0FF_EE00;

#[repr(C)]
pub(crate) struct Node<T> {
    canary: usize,
    value: Option<T>,
    pub(crate) next: AtomicUsize,
}

impl<T> Node<T> {
    pub(crate) fn dummy() -> *mut Self {
        Self::alloc_value(None)
    }

    pub(crate) fn alloc(value: T) -> *mut Self {
        Self::alloc_value(Some(value))
    }

    fn alloc_value(value: Option<T>) -> *mut Self {
        Box::into_raw(Box::new(Self {
            canary: CANARY,
            value,
            next: AtomicUsize::new(tagged::pack::<Self>(std::ptr::null_mut(), 0, false)),
        }))
    }
}

/// # Safety
///
/// `ptr` is a node of this queue, and it has not been freed yet.
pub(crate) unsafe fn drop_node<T>(ptr: *mut u8) {
    let node = ptr.cast::<Node<T>>();
    unsafe {
        assert_eq!((*node).canary, CANARY, "double free of a queue node");
        (*node).canary = 0;
        drop(Box::from_raw(node));
    }
}

pub(crate) unsafe fn load_next<T>(ptr: *mut Node<T>) -> usize {
    // `Acquire` sees the value and the generation published by the link's `Release` CAS.
    let next = unsafe { &raw const (*ptr).next };
    unsafe { (*next).load(Ordering::Acquire) }
}

pub(crate) unsafe fn cas_next<T>(
    ptr: *mut Node<T>,
    current: usize,
    new: usize,
) -> Result<usize, usize> {
    let next = unsafe { &raw const (*ptr).next };
    // `Release` publishes the new node's fields. A failed CAS rereads the word with `Acquire`.
    unsafe { (*next).compare_exchange(current, new, Ordering::Release, Ordering::Acquire) }
}

pub(crate) unsafe fn take_value<T>(ptr: *mut Node<T>) -> Option<T> {
    let value = unsafe { &raw mut (*ptr).value };
    unsafe { (*value).take() }
}
