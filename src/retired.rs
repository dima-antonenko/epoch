//! A node that has already been unlinked and has not yet been destroyed.
//!
//! The record sits in the thread's local limbo list or in the domain's
//! orphan list. Only the scheme destroys it, once the barrier says no
//! reader remains. `Drop` for `Retired` does not give the memory back: a
//! lost record leaks, and it is not freed twice. Drop a collection after
//! joining the threads that used it. Otherwise limbo that still belongs to
//! a live thread can outlive the domain.

/// Sole ownership of a block the scheme will free later.
///
/// `Retired` itself frees nothing on `Drop`: a lost record leaks, and it is
/// not destroyed twice.
pub struct Retired {
    pub(crate) ptr: *mut u8,
    pub(crate) drop_fn: unsafe fn(*mut u8),
}

// The pointer moves between threads only through the domain's limbo lists
// and is freed exactly once.
unsafe impl Send for Retired {}

impl Retired {
    /// # Safety
    ///
    /// `ptr` was allocated as the type `drop_fn` understands. After the call
    /// nobody else uses the pointer. `drop_fn` runs exactly once and takes
    /// ownership of the value.
    pub unsafe fn new(ptr: *mut u8, drop_fn: unsafe fn(*mut u8)) -> Self {
        Self { ptr, drop_fn }
    }

    pub(crate) unsafe fn free(self) {
        // SAFETY: the contract of `Retired::new`.
        unsafe { (self.drop_fn)(self.ptr) };
    }
}
