//! Epoch-based reclamation with three bags.
//!
//! On entry a thread publishes the global epoch and holds it for as long as
//! the guard is alive. Nodes retired in epoch `e` sit in bag `e % 3` and are
//! destroyed only once the global epoch has reached `e + 2`. While a guard
//! is published in epoch `e`, the global counter cannot move past `e + 1`,
//! so that guard's references are still live.
//!
//! A long pin holds reclamation back. That is the scheme's deliberate cost.

use std::cell::RefCell;
use std::marker::PhantomData;

use crate::atomic::{Arc, AtomicUsize, Mutex, Ordering, fence};
use crate::pad::CachePadded;
use crate::retired::Retired;

use super::{INACTIVE, MAX_THREADS};

struct EpochSlot {
    occupied: AtomicUsize,
    local_epoch: AtomicUsize,
}

impl EpochSlot {
    fn new() -> Self {
        Self {
            occupied: AtomicUsize::new(0),
            local_epoch: AtomicUsize::new(INACTIVE),
        }
    }
}

/// Epoch-based reclamation domain.
pub struct EpochDomain {
    global: AtomicUsize,
    slots: [CachePadded<EpochSlot>; MAX_THREADS],
    orphans: Mutex<Vec<(usize, Retired)>>,
}

struct EpochLocal {
    domain: Arc<EpochDomain>,
    index: usize,
    depth: usize,
    bags: [Vec<Retired>; 3],
    /// Epoch that owns the nodes in `bags[i]`. Meaningful when the bag is not empty.
    bag_epoch: [usize; 3],
    overflow: Vec<(usize, Retired)>,
}

impl Drop for EpochLocal {
    fn drop(&mut self) {
        #[cfg(not(feature = "loom"))]
        {
            let domain = Arc::clone(&self.domain);
            let index = self.index;
            domain.abandon(index, self);
        }
    }
}

#[cfg(not(feature = "loom"))]
std::thread_local! {
    static LOCALS: RefCell<Vec<EpochLocal>> = const { RefCell::new(Vec::new()) };
}
#[cfg(feature = "loom")]
loom::thread_local! {
    static LOCALS: RefCell<Vec<EpochLocal>> = RefCell::new(Vec::new());
}

/// A pin of the current thread. Until the guard is dropped, the thread
/// publishes an epoch.
pub struct EpochGuard<'a> {
    domain: &'a EpochDomain,
    index: usize,
    outer: bool,
    _unsend: PhantomData<*const ()>,
}

impl EpochDomain {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            global: AtomicUsize::new(0),
            slots: std::array::from_fn(|_| CachePadded::new(EpochSlot::new())),
            orphans: Mutex::new(Vec::new()),
        })
    }

    /// Enters the critical section for reading and writing pointers of this domain.
    pub fn pin(domain: &Arc<Self>) -> EpochGuard<'_> {
        let index = register(domain);
        let outer = with_local(domain, |local| {
            local.depth += 1;
            local.depth == 1
        });
        if outer {
            domain.publish(index);
        }
        EpochGuard {
            domain,
            index,
            outer,
            _unsend: PhantomData,
        }
    }

    /// Advances the epoch when every pin allows it, and destroys bags that are ready.
    ///
    /// Two steps are enough for the bag of epoch `e - 2` to become ready when
    /// the global number was `e`. The second step often does not happen: a
    /// thread that still holds a pin in epoch `e` is, after the first
    /// advance, strictly behind the global number and stops the counter. A
    /// node retired in the current epoch therefore survives this call.
    pub fn try_reclaim(&self) {
        for _ in 0..2 {
            if !self.try_advance() {
                break;
            }
        }
        self.collect(false);
    }

    pub(crate) fn free_all_for_shutdown(&self) {
        let mut doomed = Vec::new();
        let _ = LOCALS.try_with(|cell| {
            if let Ok(mut locals) = cell.try_borrow_mut() {
                if let Some(pos) = locals
                    .iter()
                    .position(|local| Arc::as_ptr(&local.domain) == self as *const EpochDomain)
                {
                    for bag in &mut locals[pos].bags {
                        doomed.append(bag);
                    }
                    doomed.extend(locals[pos].overflow.drain(..).map(|(_, item)| item));
                    locals.swap_remove(pos);
                }
            }
        });
        {
            let mut orphans = self.orphans.lock().unwrap_or_else(|err| err.into_inner());
            doomed.extend(orphans.drain(..).map(|(_, item)| item));
        }
        for item in doomed {
            unsafe { item.free() };
        }
    }

    fn publish(&self, index: usize) {
        let slot = &self.slots[index];
        loop {
            let epoch = self.global.load(Ordering::Acquire);
            // `Release`: the epoch scan sees that we entered before our
            // reads of the structure. The `SeqCst` fence keeps those reads
            // from floating above the store.
            slot.local_epoch.store(epoch, Ordering::Release);
            fence(Ordering::SeqCst);
            if self.global.load(Ordering::Acquire) == epoch {
                break;
            }
        }
    }

    fn try_advance(&self) -> bool {
        let global = self.global.load(Ordering::Acquire);
        // The same full fence as `pin`: either we see the reader's local
        // epoch, or the reader's next load sees the new global number.
        fence(Ordering::SeqCst);
        for slot in &self.slots {
            if slot.occupied.load(Ordering::Acquire) == 0 {
                continue;
            }
            let local = slot.local_epoch.load(Ordering::Acquire);
            if local != INACTIVE && local != global {
                return false;
            }
        }
        // `SeqCst` meets the pin fence in one total order of fences.
        self.global
            .compare_exchange(
                global,
                global.wrapping_add(1),
                Ordering::SeqCst,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn collect(&self, force: bool) {
        let global = self.global.load(Ordering::Acquire);
        let mut doomed = Vec::new();
        LOCALS.with(|cell| {
            let mut locals = cell.borrow_mut();
            if let Some(local) = find_mut(&mut locals, self) {
                drain_ready(local, global, force, &mut doomed);
            }
            let mut orphans = self.orphans.lock().unwrap_or_else(|err| err.into_inner());
            let mut kept = Vec::new();
            for (epoch, item) in orphans.drain(..) {
                if force || global.saturating_sub(epoch) >= 2 {
                    doomed.push(item);
                } else {
                    kept.push((epoch, item));
                }
            }
            *orphans = kept;
        });
        for item in doomed {
            unsafe { item.free() };
        }
    }

    #[cfg(not(feature = "loom"))]
    fn abandon(&self, index: usize, local: &mut EpochLocal) {
        self.slots[index]
            .local_epoch
            .store(INACTIVE, Ordering::Release);
        {
            let mut orphans = self.orphans.lock().unwrap_or_else(|err| err.into_inner());
            for idx in 0..3 {
                let epoch = local.bag_epoch[idx];
                orphans.extend(local.bags[idx].drain(..).map(|item| (epoch, item)));
            }
            orphans.append(&mut local.overflow);
        }
        self.slots[index].occupied.store(0, Ordering::Release);
        for _ in 0..2 {
            if !self.try_advance() {
                break;
            }
        }
        self.collect_orphans_only();
    }

    #[cfg(not(feature = "loom"))]
    fn collect_orphans_only(&self) {
        let global = self.global.load(Ordering::Acquire);
        let mut doomed = Vec::new();
        {
            let mut orphans = self.orphans.lock().unwrap_or_else(|err| err.into_inner());
            let mut kept = Vec::new();
            for (epoch, item) in orphans.drain(..) {
                if global.saturating_sub(epoch) >= 2 {
                    doomed.push(item);
                } else {
                    kept.push((epoch, item));
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
                // The slot is ours: the previous owner stored `INACTIVE`
                // before releasing it.
                slot.local_epoch.store(INACTIVE, Ordering::Release);
                return index;
            }
        }
        panic!("epoch: a domain admits at most {MAX_THREADS} threads");
    }
}

impl EpochGuard<'_> {
    /// Defers a node into the bag of the current global epoch.
    ///
    /// # Safety
    ///
    /// The node has already been unlinked. Threads that could have read it
    /// hold a pin no newer than the epoch in which that read was still possible.
    pub unsafe fn retire(&self, item: Retired) {
        let epoch = self.domain.global.load(Ordering::Acquire);
        with_local(self.domain, |local| {
            push_bag(local, epoch, item);
        });
        self.domain.try_reclaim();
    }
}

impl Drop for EpochGuard<'_> {
    fn drop(&mut self) {
        let depth = with_local(self.domain, |local| {
            local.depth = local.depth.saturating_sub(1);
            local.depth
        });
        if self.outer {
            debug_assert_eq!(depth, 0);
            // `Release`: an epoch advance does not overtake our reads and `retire`.
            self.domain.slots[self.index]
                .local_epoch
                .store(INACTIVE, Ordering::Release);
            self.domain.try_reclaim();
        }
    }
}

fn push_bag(local: &mut EpochLocal, epoch: usize, item: Retired) {
    let idx = epoch % 3;
    if local.bags[idx].is_empty() {
        local.bag_epoch[idx] = epoch;
    } else if local.bag_epoch[idx] != epoch {
        // Slot `e % 3` is still occupied by nodes from another epoch: this
        // thread skipped a collection and the global number moved more than
        // two steps ahead. The old nodes go to overflow with their own
        // number and are not mixed into the new epoch.
        let old = local.bag_epoch[idx];
        local
            .overflow
            .extend(local.bags[idx].drain(..).map(|retired| (old, retired)));
        local.bag_epoch[idx] = epoch;
    }
    local.bags[idx].push(item);
}

fn drain_ready(local: &mut EpochLocal, global: usize, force: bool, doomed: &mut Vec<Retired>) {
    for idx in 0..3 {
        if local.bags[idx].is_empty() {
            continue;
        }
        if force || global.saturating_sub(local.bag_epoch[idx]) >= 2 {
            doomed.extend(local.bags[idx].drain(..));
        }
    }
    let mut kept = Vec::new();
    for (epoch, item) in local.overflow.drain(..) {
        if force || global.saturating_sub(epoch) >= 2 {
            doomed.push(item);
        } else {
            kept.push((epoch, item));
        }
    }
    local.overflow = kept;
}

fn register(domain: &Arc<EpochDomain>) -> usize {
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
        locals.push(EpochLocal {
            domain: Arc::clone(domain),
            index,
            depth: 0,
            bags: [Vec::new(), Vec::new(), Vec::new()],
            bag_epoch: [0, 0, 0],
            overflow: Vec::new(),
        });
        index
    })
}

fn with_local<R>(domain: &EpochDomain, f: impl FnOnce(&mut EpochLocal) -> R) -> R {
    LOCALS.with(|cell| {
        let mut locals = cell.borrow_mut();
        let local = find_mut(&mut locals, domain).expect("thread has not entered the epoch domain");
        f(local)
    })
}

fn find_mut<'a>(locals: &'a mut [EpochLocal], domain: &EpochDomain) -> Option<&'a mut EpochLocal> {
    let ptr = domain as *const EpochDomain;
    locals
        .iter_mut()
        .find(|local| Arc::as_ptr(&local.domain) == ptr)
}
