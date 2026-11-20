//! Atomics, the shared pointer, and the mutex: `std` in a normal build and
//! `loom` when the `loom` feature is on.
//!
//! The two sources must not be mixed in one scenario. `cargo test` runs
//! `std`. `cargo test --features loom` runs the `loom` model.

#[cfg(not(feature = "loom"))]
pub use std::sync::Mutex;
#[cfg(not(feature = "loom"))]
pub use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering, fence};

#[cfg(feature = "loom")]
pub use loom::sync::Mutex;
#[cfg(feature = "loom")]
pub use loom::sync::atomic::{AtomicPtr, AtomicUsize, Ordering, fence};

/// The domain handle is always an ordinary `Arc`.
///
/// `loom::sync::Arc` must not be dropped from a TLS destructor: by then the
/// model has already finished, and touching its primitives fails the test.
/// The local record keeps the domain alive until the thread has exited.
pub use std::sync::Arc;

/// Yield the thread. Under `loom` this is a model switch point; otherwise it
/// is an ordinary spin hint.
#[cfg(not(feature = "loom"))]
pub fn spin_loop() {
    std::hint::spin_loop();
}

#[cfg(feature = "loom")]
pub fn spin_loop() {
    loom::thread::yield_now();
}
