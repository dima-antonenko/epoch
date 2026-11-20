//! A node of a sorted Michael list.
//!
//! The low bit of `next` is logical deletion. The top 16 bits are the
//! generation for a CAS on the link. The key and the value do not change
//! after publication: the table does not update an existing key.

use crate::atomic::{AtomicUsize, Ordering};
use crate::tagged;

/// The canary catches a repeated `free` of a table node.
pub(crate) const CANARY: usize = 0xA11C_E5A1_1CE5_A110;

#[repr(C)]
pub(crate) struct Node<K, V> {
    canary: usize,
    key: K,
    value: V,
    pub(crate) next: AtomicUsize,
}

impl<K, V> Node<K, V> {
    pub(crate) fn alloc(key: K, value: V) -> *mut Self {
        Box::into_raw(Box::new(Self {
            canary: CANARY,
            key,
            value,
            next: AtomicUsize::new(tagged::pack::<Self>(std::ptr::null_mut(), 0, false)),
        }))
    }
}

/// # Safety
///
/// `ptr` was allocated by [`Node::alloc`] and has not been freed yet.
pub(crate) unsafe fn drop_node<K, V>(ptr: *mut u8) {
    let node = ptr.cast::<Node<K, V>>();
    unsafe {
        assert_eq!((*node).canary, CANARY, "double free of a map node");
        (*node).canary = 0;
        drop(Box::from_raw(node));
    }
}

pub(crate) unsafe fn key<'a, K, V>(ptr: *mut Node<K, V>) -> &'a K {
    let key = unsafe { &raw const (*ptr).key };
    unsafe { &*key }
}

pub(crate) unsafe fn clone_value<K, V: Clone>(ptr: *mut Node<K, V>) -> V {
    let value = unsafe { &raw const (*ptr).value };
    unsafe { (*value).clone() }
}

pub(crate) unsafe fn load_next<K, V>(ptr: *mut Node<K, V>) -> usize {
    // `Acquire` sees the mark and the generation written by the link's `Release` CAS.
    let next = unsafe { &raw const (*ptr).next };
    unsafe { (*next).load(Ordering::Acquire) }
}

pub(crate) unsafe fn store_next<K, V>(ptr: *mut Node<K, V>, raw: usize) {
    // The node is not published yet. The predecessor's `Release` CAS publishes the link.
    let next = unsafe { &raw const (*ptr).next };
    unsafe { (*next).store(raw, Ordering::Relaxed) };
}

pub(crate) unsafe fn cas_link<K, V>(
    link: *const AtomicUsize,
    current: usize,
    new: usize,
) -> Result<usize, usize> {
    // `Release` publishes a new node or a deletion mark.
    unsafe { (*link).compare_exchange(current, new, Ordering::Release, Ordering::Acquire) }
}

pub(crate) fn bucket_index<K: std::hash::Hash>(key: &K, len: usize) -> usize {
    // `DefaultHasher` and a remainder. The bucket count is fixed, so there
    // is no rehash when the table would otherwise grow.
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    Hash::hash(key, &mut hasher);
    (hasher.finish() as usize) % len
}
