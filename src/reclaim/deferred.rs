//! Deferred reclamation by tickets.
//!
//! Unlike epoch, there are no three bags and no global epoch to advance.
//! `retire` takes a monotonic ticket. On entry to a section the thread
//! publishes the latest ticket it has seen; on exit it writes "inactive".
//! A node is destroyed when its ticket is strictly less than the minimum
//! over the active slots: every participant has either entered a section
//! after this retire, or has left its section and holds no references.

use std::cell::RefCell;
use std::marker::PhantomData;

use crate::atomic::{Arc, AtomicUsize, Mutex, Ordering, fence};
use crate::pad::CachePadded;
use crate::retired::Retired;

use super::{INACTIVE, MAX_THREADS};

struct Slot {
    occupied: AtomicUsize,
    clock: AtomicUsize,
}

impl Slot {
    fn new() -> Self {
        Self {
            occupied: AtomicUsize::new(0),
            clock: AtomicUsize::new(INACTIVE),
        }
    }
}

struct Stamped {
    stamp: usize,
    item: Retired,
}

/// Deferred-reclamation domain keyed by the minimum thread label.
pub struct DeferredDomain {
    ticket: AtomicUsize,
    slots: [CachePadded<Slot>; MAX_THREADS],
    orphans: Mutex<Vec<Stamped>>,
}

struct DeferredLocal {
    domain: Arc<DeferredDomain>,
    index: usize,
    depth: usize,
    limbo: Vec<Stamped>,
}

impl Drop for DeferredLocal {
    fn drop(&mut self) {
        #[cfg(not(feature = "loom"))]
        self.domain.abandon(self.index, &mut self.limbo);
    }
}

#[cfg(not(feature = "loom"))]
std::thread_local! {
    static LOCALS: RefCell<Vec<DeferredLocal>> = const { RefCell::new(Vec::new()) };
}
#[cfg(feature = "loom")]
loom::thread_local! {
    static LOCALS: RefCell<Vec<DeferredLocal>> = RefCell::new(Vec::new());
}

/// A section inside which the thread publishes its label.
pub struct DeferredGuard<'a> {
    domain: &'a DeferredDomain,
    index: usize,
    outer: bool,
    _unsend: PhantomData<*const ()>,
}

impl DeferredDomain {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            ticket: AtomicUsize::new(0),
            slots: std::array::from_fn(|_| CachePadded::new(Slot::new())),
            orphans: Mutex::new(Vec::new()),
        })
    }

    /// Publishes the label before any read of the shared structure.
    pub fn enter(domain: &Arc<Self>) -> DeferredGuard<'_> {
        let index = register(domain);
        let outer = with_local(domain, |local| {
            local.depth += 1;
            local.depth == 1
        });
        if outer {
            let ticket = domain.ticket.load(Ordering::Acquire);
            // `Release` plus a full fence: reads of the structure do not
            // rise above the published label, and a scanner either sees the
            // label or has its unlink observed by those reads.
            domain.slots[index].clock.store(ticket, Ordering::Release);
            fence(Ordering::SeqCst);
        }
        DeferredGuard {
            domain,
            index,
            outer,
            _unsend: PhantomData,
        }
    }

    /// Puts a node into limbo with a fresh ticket and tries to free the ready prefix.
    ///
    /// The thread must already have entered the domain: limbo lives in its TLS.
    ///
    /// # Safety
    ///
    /// The node has already been unlinked. Threads that could have read it
    /// have published a label no newer than this ticket, or they enter later
    /// and do not see the node.
    pub unsafe fn retire(&self, item: Retired) {
        // `Release`: an unlink performed earlier in this thread is visible
        // to a thread that `Acquire`-loaded the ticket and only then reads
        // the structure.
        let stamp = self.ticket.fetch_add(1, Ordering::Release);
        with_local(self, |local| {
            local.limbo.push(Stamped { stamp, item });
        });
        self.try_reclaim();
    }

    /// Destroys nodes whose stamp is strictly less than the minimum active label.
    pub fn try_reclaim(&self) {
        fence(Ordering::SeqCst);
        let min = self.min_clock();
        let mut doomed = Vec::new();
        LOCALS.with(|cell| {
            let mut locals = cell.borrow_mut();
            if let Some(local) = find_mut(&mut locals, self) {
                let mut kept = Vec::new();
                for stamped in local.limbo.drain(..) {
                    // Strictly less: a stamp equal to a published label is
                    // one the thread may have observed before it saw this
                    // node's unlink.
                    if stamped.stamp < min {
                        doomed.push(stamped.item);
                    } else {
                        kept.push(stamped);
                    }
                }
                local.limbo = kept;
            }
            let mut orphans = self.orphans.lock().unwrap_or_else(|err| err.into_inner());
            let mut kept = Vec::new();
            for stamped in orphans.drain(..) {
                if stamped.stamp < min {
                    doomed.push(stamped.item);
                } else {
                    kept.push(stamped);
                }
            }
            *orphans = kept;
        });
        for item in doomed {
            unsafe { item.free() };
        }
    }

    pub(crate) fn free_all_for_shutdown(&self) {
        let mut doomed = Vec::new();
        let _ = LOCALS.try_with(|cell| {
            if let Ok(mut locals) = cell.try_borrow_mut() {
                if let Some(pos) = locals
                    .iter()
                    .position(|local| Arc::as_ptr(&local.domain) == self as *const DeferredDomain)
                {
                    doomed.extend(locals[pos].limbo.drain(..).map(|stamped| stamped.item));
                    locals.swap_remove(pos);
                }
            }
        });
        {
            let mut orphans = self.orphans.lock().unwrap_or_else(|err| err.into_inner());
            doomed.extend(orphans.drain(..).map(|stamped| stamped.item));
        }
        for item in doomed {
            unsafe { item.free() };
        }
    }

    fn min_clock(&self) -> usize {
        // A free slot does not take part in the minimum. An occupied but
        // inactive slot stores `INACTIVE` (`usize::MAX`) and does not lower
        // the minimum either: a thread outside a section holds no
        // references. If there are no sections at all, the minimum stays
        // `INACTIVE`, and any stamp strictly below it may be destroyed.
        let mut min = INACTIVE;
        for slot in &self.slots {
            if slot.occupied.load(Ordering::Acquire) == 0 {
                continue;
            }
            let clock = slot.clock.load(Ordering::Acquire);
            if clock < min {
                min = clock;
            }
        }
        min
    }

    #[cfg(not(feature = "loom"))]
    fn abandon(&self, index: usize, limbo: &mut Vec<Stamped>) {
        self.slots[index].clock.store(INACTIVE, Ordering::Release);
        if !limbo.is_empty() {
            let mut orphans = self.orphans.lock().unwrap_or_else(|err| err.into_inner());
            orphans.append(limbo);
        }
        self.slots[index].occupied.store(0, Ordering::Release);
        self.reclaim_orphans_only();
    }

    #[cfg(not(feature = "loom"))]
    fn reclaim_orphans_only(&self) {
        fence(Ordering::SeqCst);
        let min = self.min_clock();
        let mut doomed = Vec::new();
        {
            let mut orphans = self.orphans.lock().unwrap_or_else(|err| err.into_inner());
            let mut kept = Vec::new();
            for stamped in orphans.drain(..) {
                if stamped.stamp < min {
                    doomed.push(stamped.item);
                } else {
                    kept.push(stamped);
                }
            }
            *orphans = kept;
        }
        for item in doomed {
            unsafe { item.free() };
        }
    }

    fn claim_slot(&self) -> usize {
        for (index, slot) in self.slots.iter().enumerate() {
            if slot
                .occupied
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                slot.clock.store(INACTIVE, Ordering::Release);
                return index;
            }
        }
        panic!("epoch: a domain admits at most {MAX_THREADS} threads");
    }
}

impl DeferredGuard<'_> {
    /// Defers a node without leaving the section. This thread's own label
    /// keeps the node alive until the guard's `Drop`.
    ///
    /// # Safety
    ///
    /// The same contract as [`DeferredDomain::retire`].
    pub unsafe fn retire(&self, item: Retired) {
        // SAFETY: the method's contract.
        unsafe { self.domain.retire(item) };
    }
}

impl Drop for DeferredGuard<'_> {
    fn drop(&mut self) {
        if self.outer {
            self.domain.slots[self.index]
                .clock
                .store(INACTIVE, Ordering::Release);
        }
        with_local(self.domain, |local| {
            local.depth = local.depth.saturating_sub(1);
        });
        if self.outer {
            self.domain.try_reclaim();
        }
    }
}

fn register(domain: &Arc<DeferredDomain>) -> usize {
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
        locals.push(DeferredLocal {
            domain: Arc::clone(domain),
            index,
            depth: 0,
            limbo: Vec::new(),
        });
        index
    })
}

fn with_local<R>(domain: &DeferredDomain, f: impl FnOnce(&mut DeferredLocal) -> R) -> R {
    LOCALS.with(|cell| {
        let mut locals = cell.borrow_mut();
        let local =
            find_mut(&mut locals, domain).expect("thread has not entered the deferred domain");
        f(local)
    })
}

fn find_mut<'a>(
    locals: &'a mut [DeferredLocal],
    domain: &DeferredDomain,
) -> Option<&'a mut DeferredLocal> {
    let ptr = domain as *const DeferredDomain;
    locals
        .iter_mut()
        .find(|local| Arc::as_ptr(&local.domain) == ptr)
}
