//! Userspace RCU with two reader counters.
//!
//! This is not epoch. A read-side section enters the current cohort: it
//! increments that cohort's counter and checks that the cohort index has
//! not changed. Otherwise it rolls the increment back. `retire` switches
//! the cohort with a `SeqCst` barrier and waits until the previous counter
//! is zero. After that, nodes deferred into the old cohort may be destroyed.
//!
//! `retire` must not be called from a read-side section of the same thread:
//! `synchronize` would wait on our own counter.

use std::cell::RefCell;
use std::marker::PhantomData;

use crate::atomic::{Arc, AtomicUsize, Mutex, Ordering, fence, spin_loop};
use crate::pad::CachePadded;
use crate::retired::Retired;

use super::MAX_THREADS;

struct Slot {
    occupied: AtomicUsize,
}

/// RCU domain.
pub struct RcuDomain {
    current: AtomicUsize,
    readers: [CachePadded<AtomicUsize>; 2],
    queues: [Mutex<Vec<Retired>>; 2],
    synchronizing: AtomicUsize,
    slots: [CachePadded<Slot>; MAX_THREADS],
}

struct RcuLocal {
    domain: Arc<RcuDomain>,
    index: usize,
    depth: usize,
    cohort: usize,
}

impl Drop for RcuLocal {
    fn drop(&mut self) {
        #[cfg(not(feature = "loom"))]
        {
            let domain = Arc::clone(&self.domain);
            domain.abandon(self);
        }
    }
}

#[cfg(not(feature = "loom"))]
std::thread_local! {
    static LOCALS: RefCell<Vec<RcuLocal>> = const { RefCell::new(Vec::new()) };
}
#[cfg(feature = "loom")]
loom::thread_local! {
    static LOCALS: RefCell<Vec<RcuLocal>> = RefCell::new(Vec::new());
}

/// A read-side section. A nested guard only increments the depth.
pub struct RcuGuard<'a> {
    domain: &'a RcuDomain,
    cohort: usize,
    outer: bool,
    _unsend: PhantomData<*const ()>,
}

impl RcuDomain {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            current: AtomicUsize::new(0),
            readers: std::array::from_fn(|_| CachePadded::new(AtomicUsize::new(0))),
            queues: std::array::from_fn(|_| Mutex::new(Vec::new())),
            synchronizing: AtomicUsize::new(0),
            slots: std::array::from_fn(|_| {
                CachePadded::new(Slot {
                    occupied: AtomicUsize::new(0),
                })
            }),
        })
    }

    /// Enters a read-side section.
    pub fn lock(domain: &Arc<Self>) -> RcuGuard<'_> {
        let _index = register(domain);
        let (outer, cohort) = with_local(domain, |local| {
            if local.depth == 0 {
                let cohort = domain.acquire_cohort();
                local.cohort = cohort;
                local.depth = 1;
                (true, cohort)
            } else {
                local.depth += 1;
                (false, local.cohort)
            }
        });
        RcuGuard {
            domain,
            cohort,
            outer,
            _unsend: PhantomData,
        }
    }

    /// Puts a node on the current cohort's queue and tries to finish a grace period.
    ///
    /// # Safety
    ///
    /// The node has already been unlinked. Readers that could have seen it
    /// are in a read-side section that began before the unlink.
    ///
    /// # Panics
    ///
    /// Panics when called from a read-side section of this same thread.
    pub unsafe fn retire(&self, item: Retired) {
        if self.depth() != 0 {
            panic!(
                "RCU: retire inside a read-side critical section would wait on this thread's own reader count"
            );
        }
        let cohort = self.current.load(Ordering::Acquire) & 1;
        self.queues[cohort]
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(item);
        self.try_sync();
    }

    /// Tries to close a grace period, unless another thread is already doing that.
    pub fn try_reclaim(&self) {
        self.try_sync();
    }

    pub(crate) fn free_all_for_shutdown(&self) {
        let _ = LOCALS.try_with(|cell| {
            if let Ok(mut locals) = cell.try_borrow_mut() {
                if let Some(pos) = locals
                    .iter()
                    .position(|local| Arc::as_ptr(&local.domain) == self as *const RcuDomain)
                {
                    locals.swap_remove(pos);
                }
            }
        });
        debug_assert_eq!(self.readers[0].load(Ordering::Acquire), 0);
        debug_assert_eq!(self.readers[1].load(Ordering::Acquire), 0);
        let mut doomed = Vec::new();
        for queue in &self.queues {
            doomed.append(&mut queue.lock().unwrap_or_else(|err| err.into_inner()));
        }
        for item in doomed {
            unsafe { item.free() };
        }
    }

    fn acquire_cohort(&self) -> usize {
        loop {
            let cohort = self.current.load(Ordering::Acquire) & 1;
            // `AcqRel`: publishing the entry does not sink below the later
            // reads of the structure.
            self.readers[cohort].fetch_add(1, Ordering::AcqRel);
            fence(Ordering::SeqCst);
            if self.current.load(Ordering::Acquire) & 1 == cohort {
                return cohort;
            }
            self.readers[cohort].fetch_sub(1, Ordering::Release);
            spin_loop();
        }
    }

    fn try_sync(&self) {
        if self
            .synchronizing
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        // This cohort's queue already holds nodes unlinked before the switch.
        // The next `retire` writes the other queue, and this pass does not
        // touch those nodes.
        let cohort = self.current.load(Ordering::Acquire) & 1;
        let next = cohort ^ 1;
        // `SeqCst` places the switch in the same total order as the reader's
        // fence. A reader that incremented the old counter and has not yet
        // rechecked the index either rolls back or is counted by the wait
        // below.
        self.current.store(next, Ordering::SeqCst);
        while self.readers[cohort].load(Ordering::Acquire) != 0 {
            spin_loop();
        }
        let batch = {
            let mut queue = self.queues[cohort]
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            std::mem::take(&mut *queue)
        };
        self.synchronizing.store(0, Ordering::Release);
        for item in batch {
            unsafe { item.free() };
        }
    }

    fn depth(&self) -> usize {
        LOCALS.with(|cell| {
            let locals = cell.borrow();
            find(&locals, self).map(|local| local.depth).unwrap_or(0)
        })
    }

    #[cfg(not(feature = "loom"))]
    fn abandon(&self, local: &mut RcuLocal) {
        if local.depth > 0 {
            self.readers[local.cohort].fetch_sub(1, Ordering::Release);
            local.depth = 0;
        }
        self.slots[local.index].occupied.store(0, Ordering::Release);
    }

    fn claim_slot(&self) -> usize {
        for (index, slot) in self.slots.iter().enumerate() {
            if slot
                .occupied
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return index;
            }
        }
        panic!("epoch: a domain admits at most {MAX_THREADS} threads");
    }
}

impl Drop for RcuGuard<'_> {
    fn drop(&mut self) {
        if self.outer {
            // Leave the counter first, then decrement the depth. The other
            // order would let this thread's `retire` start a synchronize
            // while the thread is still counted as a reader.
            self.domain.readers[self.cohort].fetch_sub(1, Ordering::Release);
        }
        with_local(self.domain, |local| {
            local.depth = local.depth.saturating_sub(1);
        });
    }
}

fn register(domain: &Arc<RcuDomain>) -> usize {
    LOCALS.with(|cell| {
        let mut locals = cell.borrow_mut();
        let ptr = Arc::as_ptr(domain);
        if let Some(local) = locals
            .iter()
            .find(|local| Arc::as_ptr(&local.domain) == ptr)
        {
            return local.index;
        }
        let index = domain.claim_slot();
        locals.push(RcuLocal {
            domain: Arc::clone(domain),
            index,
            depth: 0,
            cohort: 0,
        });
        index
    })
}

fn with_local<R>(domain: &RcuDomain, f: impl FnOnce(&mut RcuLocal) -> R) -> R {
    LOCALS.with(|cell| {
        let mut locals = cell.borrow_mut();
        let local = find_mut(&mut locals, domain).expect("thread has not entered the RCU domain");
        f(local)
    })
}

fn find<'a>(locals: &'a [RcuLocal], domain: &RcuDomain) -> Option<&'a RcuLocal> {
    let ptr = domain as *const RcuDomain;
    locals
        .iter()
        .find(|local| Arc::as_ptr(&local.domain) == ptr)
}

fn find_mut<'a>(locals: &'a mut [RcuLocal], domain: &RcuDomain) -> Option<&'a mut RcuLocal> {
    let ptr = domain as *const RcuDomain;
    locals
        .iter_mut()
        .find(|local| Arc::as_ptr(&local.domain) == ptr)
}
