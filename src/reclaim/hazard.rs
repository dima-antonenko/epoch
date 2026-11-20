//! Hazard pointers (Maged Michael).
//!
//! Before a thread dereferences a node, it publishes the node's address in
//! its slot and reads the atomic again. A `SeqCst` fence keeps that check
//! from floating above the publication. Reclamation scans every slot and
//! destroys only addresses that are absent from the scan.
//!
//! A thread has two slots. The stack needs one. The queue needs the head
//! and the next node at the same time.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::ptr;

use crate::atomic::{Arc, AtomicPtr, AtomicUsize, Mutex, Ordering, fence};
use crate::pad::CachePadded;
use crate::retired::Retired;
use crate::tagged;

use super::MAX_THREADS;

const HAZARD_SLOTS: usize = 2;
/// Amortized threshold from the paper: twice the number of published slots.
const RETIRE_THRESHOLD: usize = 2 * MAX_THREADS * HAZARD_SLOTS;

struct HazardSlot {
    occupied: AtomicUsize,
    hazards: [AtomicPtr<()>; HAZARD_SLOTS],
}

impl HazardSlot {
    fn new() -> Self {
        Self {
            occupied: AtomicUsize::new(0),
            hazards: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
        }
    }
}

/// Hazard-pointer domain. A collection keeps it in an `Arc`.
pub struct HazardDomain {
    slots: [CachePadded<HazardSlot>; MAX_THREADS],
    orphans: Mutex<Vec<Retired>>,
}

struct HazardLocal {
    domain: Arc<HazardDomain>,
    index: usize,
    depth: usize,
    retired: Vec<Retired>,
}

impl Drop for HazardLocal {
    fn drop(&mut self) {
        // A loom thread's TLS is destroyed outside the model, so those
        // atomics must not be touched there. The shield's `Drop` clears the
        // published slot while the model is still alive.
        #[cfg(not(feature = "loom"))]
        self.domain.abandon(self.index, &mut self.retired);
    }
}

#[cfg(not(feature = "loom"))]
std::thread_local! {
    static LOCALS: RefCell<Vec<HazardLocal>> = const { RefCell::new(Vec::new()) };
}
#[cfg(feature = "loom")]
loom::thread_local! {
    static LOCALS: RefCell<Vec<HazardLocal>> = RefCell::new(Vec::new());
}

/// The window in which a thread publishes protected pointers.
///
/// A nested `enter` of the same domain on the same thread shares the same
/// two slots and only increments the depth. The outer window clears the
/// slots: the inner `Drop` does not erase protection the outer call still
/// has to read. A nested entry has no slot set of its own, so a `Drop` of a
/// value that runs from `retire` must not enter this same domain again and
/// expect the outer pointers to still be in place.
pub struct Shield<'a> {
    domain: &'a HazardDomain,
    index: usize,
    outer: bool,
    _unsend: PhantomData<*const ()>,
}

impl HazardDomain {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            slots: std::array::from_fn(|_| CachePadded::new(HazardSlot::new())),
            orphans: Mutex::new(Vec::new()),
        })
    }

    /// Claims the thread's slot and returns a protection window.
    ///
    /// Takes `&Arc` rather than `self: &Arc<Self>`: `loom::sync::Arc` does
    /// not implement `Receiver`, and that signature does not compile in the
    /// model. The domain handle is `std::sync::Arc` either way; the note
    /// records why the method is not an `Arc` method.
    pub fn enter(domain: &Arc<Self>) -> Shield<'_> {
        let index = register(domain);
        let outer = with_local(domain, |local| {
            local.depth += 1;
            local.depth == 1
        });
        Shield {
            domain,
            index,
            outer,
            _unsend: PhantomData,
        }
    }

    /// Tries to destroy local and orphaned limbo that nobody protects anymore.
    pub fn try_reclaim(&self) {
        let batch = LOCALS.with(|cell| {
            let mut locals = cell.borrow_mut();
            if let Some(local) = find_mut(&mut locals, self) {
                self.take_unprotected(&mut local.retired)
            } else {
                Vec::new()
            }
        });
        self.free_unprotected(batch);
    }

    /// Destroys everything this thread has deferred, plus orphans. Called
    /// when nobody uses the collection anymore.
    pub(crate) fn free_all_for_shutdown(&self) {
        let mut batch = Vec::new();
        let _ = LOCALS.try_with(|cell| {
            if let Ok(mut locals) = cell.try_borrow_mut() {
                if let Some(pos) = locals
                    .iter()
                    .position(|local| Arc::as_ptr(&local.domain) == self as *const HazardDomain)
                {
                    batch.append(&mut locals[pos].retired);
                    // Drop the registration here, inside the loom model, not
                    // in the TLS destructor.
                    locals.swap_remove(pos);
                }
            }
        });
        {
            let mut orphans = self.orphans.lock().unwrap_or_else(|err| err.into_inner());
            batch.append(&mut orphans);
        }
        for item in batch {
            unsafe { item.free() };
        }
    }

    #[cfg(not(feature = "loom"))]
    fn abandon(&self, index: usize, retired: &mut Vec<Retired>) {
        let slot = &self.slots[index];
        for hazard in &slot.hazards {
            hazard.store(ptr::null_mut(), Ordering::Release);
        }
        if !retired.is_empty() {
            // The nodes are not destroyed here. Another thread's scan, or
            // the collection's `Drop`, will take them while the domain is
            // still alive. The slot itself must be released, or a departed
            // thread occupies a domain seat forever.
            let mut orphans = self.orphans.lock().unwrap_or_else(|err| err.into_inner());
            orphans.append(retired);
        }
        slot.occupied.store(0, Ordering::Release);
    }

    fn claim_slot(&self) -> usize {
        for (index, slot) in self.slots.iter().enumerate() {
            if slot
                .occupied
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                for hazard in &slot.hazards {
                    hazard.store(ptr::null_mut(), Ordering::Release);
                }
                return index;
            }
        }
        panic!("epoch: a domain admits at most {MAX_THREADS} threads");
    }

    fn take_unprotected(&self, retired: &mut Vec<Retired>) -> Vec<Retired> {
        // Paired with the fence in `protect`: if the reader has already
        // published the slot, this scan sees it; if not, the reader's
        // second load sees the unlink.
        fence(Ordering::SeqCst);
        let hazards = self.snapshot();
        let mut doomed = Vec::new();
        let local = std::mem::take(retired);
        *retired = partition(local, &hazards, &mut doomed);
        {
            let mut orphans = self.orphans.lock().unwrap_or_else(|err| err.into_inner());
            let taken = std::mem::take(&mut *orphans);
            *orphans = partition(taken, &hazards, &mut doomed);
        }
        doomed
    }

    fn free_unprotected(&self, batch: Vec<Retired>) {
        let mut defer = Vec::new();
        let hazards = self.snapshot();
        for item in batch {
            // The second check closes the window in which `protect` published
            // the address after the snapshot. A false hit on a short failed
            // `protect` attempt is safe: the node simply spends one more
            // pass in the orphan list.
            if hazards.contains(&(item.ptr as usize)) || self.is_protected(item.ptr) {
                defer.push(item);
            } else {
                unsafe { item.free() };
            }
        }
        if !defer.is_empty() {
            let mut orphans = self.orphans.lock().unwrap_or_else(|err| err.into_inner());
            orphans.append(&mut defer);
        }
    }

    fn snapshot(&self) -> Vec<usize> {
        let mut found = Vec::new();
        for slot in &self.slots {
            for hazard in &slot.hazards {
                let ptr = hazard.load(Ordering::Acquire);
                if !ptr.is_null() {
                    found.push(ptr as usize);
                }
            }
        }
        found
    }

    fn is_protected(&self, ptr: *mut u8) -> bool {
        let needle = ptr as usize;
        self.slots.iter().any(|slot| {
            slot.hazards
                .iter()
                .any(|hazard| hazard.load(Ordering::Acquire) as usize == needle)
        })
    }
}

impl<'a> Shield<'a> {
    /// Publishes the address loaded from `atomic` and returns the word only
    /// when the word is unchanged after the publication.
    ///
    /// `hazard` is 0 or 1. The stack uses zero. The queue uses both.
    pub fn protect(&self, hazard: usize, atomic: &AtomicUsize) -> usize {
        assert!(hazard < HAZARD_SLOTS, "a thread has only two hazard slots");
        let slot = &self.domain.slots[self.index].hazards[hazard];
        loop {
            let raw = atomic.load(Ordering::Acquire);
            let ptr = tagged::pointer::<()>(raw);
            // `Release` publishes the address before the fence. The fence
            // itself keeps the second `Acquire` load from happening before
            // that store.
            slot.store(ptr, Ordering::Release);
            fence(Ordering::SeqCst);
            if atomic.load(Ordering::Acquire) == raw {
                return raw;
            }
        }
    }

    /// Writes an address this thread has already validated into a slot,
    /// without rereading the list.
    ///
    /// Used when a node moves from the "current" role to the "previous"
    /// role and its protection must not lapse between the two roles.
    pub fn protect_ptr<T>(&self, hazard: usize, ptr: *mut T) {
        assert!(hazard < HAZARD_SLOTS, "a thread has only two hazard slots");
        self.domain.slots[self.index].hazards[hazard].store(ptr.cast(), Ordering::Release);
        fence(Ordering::SeqCst);
    }

    /// Clears one protected address. Reclaiming one's own node would
    /// otherwise see one's own slot and keep the block forever.
    pub fn clear(&self, hazard: usize) {
        assert!(hazard < HAZARD_SLOTS, "a thread has only two hazard slots");
        self.domain.slots[self.index].hazards[hazard].store(ptr::null_mut(), Ordering::Release);
    }

    /// Puts an unlinked node into local limbo. A scan starts at the threshold.
    ///
    /// # Safety
    ///
    /// The node is already unreachable from the structure. Live readers hold
    /// it in a hazard slot, or they have not yet passed the `protect` check
    /// and will not dereference it.
    pub unsafe fn retire(&self, item: Retired) {
        let batch = with_local(self.domain, |local| {
            local.retired.push(item);
            if local.retired.len() >= RETIRE_THRESHOLD {
                self.domain.take_unprotected(&mut local.retired)
            } else {
                Vec::new()
            }
        });
        self.domain.free_unprotected(batch);
    }
}

impl Drop for Shield<'_> {
    fn drop(&mut self) {
        let depth = with_local(self.domain, |local| {
            local.depth = local.depth.saturating_sub(1);
            local.depth
        });
        if self.outer {
            debug_assert_eq!(depth, 0);
            for hazard in &self.domain.slots[self.index].hazards {
                hazard.store(ptr::null_mut(), Ordering::Release);
            }
        }
    }
}

fn register(domain: &Arc<HazardDomain>) -> usize {
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
        locals.push(HazardLocal {
            domain: Arc::clone(domain),
            index,
            depth: 0,
            retired: Vec::new(),
        });
        index
    })
}

fn with_local<R>(domain: &HazardDomain, f: impl FnOnce(&mut HazardLocal) -> R) -> R {
    LOCALS.with(|cell| {
        let mut locals = cell.borrow_mut();
        let local = find_mut(&mut locals, domain)
            .expect("thread has not entered the hazard-pointer domain");
        f(local)
    })
}

fn find_mut<'a>(
    locals: &'a mut [HazardLocal],
    domain: &HazardDomain,
) -> Option<&'a mut HazardLocal> {
    let ptr = domain as *const HazardDomain;
    locals
        .iter_mut()
        .find(|local| Arc::as_ptr(&local.domain) == ptr)
}

fn partition(items: Vec<Retired>, hazards: &[usize], doomed: &mut Vec<Retired>) -> Vec<Retired> {
    let mut keep = Vec::new();
    for item in items {
        if hazards.contains(&(item.ptr as usize)) {
            keep.push(item);
        } else {
            doomed.push(item);
        }
    }
    keep
}
