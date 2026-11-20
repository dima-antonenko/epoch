# epoch

[English](#english) · [Русский](#русский)

## English

Teaching implementations of four safe-memory-reclamation schemes, and of the
lock-free collections built on top of them. The code is arranged so that the
algorithm sits next to the atomic: each collection has its own file, its own
CAS loop, and a comment on the memory ordering of that loop.

Public collection methods are safe. `unsafe` stays inside the schemes and
inside the structures. The invariants are written in the rustdoc (English)
and in this file (English, then Russian). The crate is meant to be read.
Production code should use `crossbeam-epoch` and industrial queues: this
crate fixes the number of threads, does not grow the hash table, wraps a
16-bit pointer generation, and takes a mutex on part of the reclamation path.

License MIT, edition 2024, `publish = false`. Pointers are 64-bit: that is
checked at compile time (`usize::BITS == 64`). The only optional dependency
is `loom` 0.7, behind the `loom` feature. There are no other dependencies.

### Contents

- [Quick start](#quick-start)
- [How to read the sources](#how-to-read-the-sources)
- [Two different problems](#two-different-problems)
- [Packing a word](#packing-a-word)
- [Memory orderings](#memory-orderings)
- [Domain, slots, and limbo](#domain-slots-and-limbo)
- [Hazard pointers](#hazard-pointers)
- [Epoch](#epoch-1)
- [RCU](#rcu)
- [Deferred](#deferred)
- [Comparing the schemes](#comparing-the-schemes)
- [Treiber stack](#treiber-stack)
- [Michael–Scott queue](#michaelscott-queue)
- [Hash map](#hash-map)
- [Public API](#public-api)
- [Unsafe contract](#unsafe-contract)
- [Panics and asserts](#panics-and-asserts)
- [Checks](#checks)
- [Repository map](#repository-map)
- [Limits](#limits)
- [Glossary](#glossary)
- [References](#references)

### Quick start

The stack, the queue, and the map are not hidden behind a shared trait.
Each scheme has its own type, so the CAS loop and the comment on the barrier
live in one file.

```rust
use epoch::{EpochMap, EpochQueue, HazardStack};

let stack = HazardStack::new();
stack.push(1);
stack.push(2);
assert_eq!(stack.pop(), Some(2));

let queue = EpochQueue::new();
queue.enqueue(1);
queue.enqueue(2);
assert_eq!(queue.dequeue(), Some(1));

let map = EpochMap::new(16); // the bucket count is fixed
assert!(map.insert(1, 10));
assert_eq!(map.get(&1), Some(10)); // the value is cloned
assert!(map.remove(&1));
```

The collections are `Send + Sync` under the bounds below, and they are shared
across threads as `Arc<HazardStack<_>>`. The examples in `examples/` run the
stack and the queue on hazard pointers:

```sh
cargo run --example stack   # prints: stack: [1, 2]
cargo run --example queue   # prints: queue: Some(1), Some(2)
```

The schemes can be called without a collection. The domain lives in an `Arc`.
The guard is tied to the thread and to the lifetime of a reference to the
domain. `retire` is unsafe: the caller must already have removed the block
from every shared structure.

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use epoch::reclaim::{HazardDomain, Retired};
use epoch::tagged;

unsafe fn free_flag(ptr: *mut u8) {
    let flag = unsafe { Box::from_raw(ptr.cast::<Arc<AtomicBool>>()) };
    flag.store(true, Ordering::SeqCst);
}

let domain = HazardDomain::new();
let flag = Arc::new(AtomicBool::new(false));
let block = Box::into_raw(Box::new(Arc::clone(&flag)));
let slot = AtomicUsize::new(tagged::pack(block, 1, false));
let shield = HazardDomain::enter(&domain);

// The word came back unchanged, so the address is published in the slot
// and the block may be read.
let raw = shield.protect(0, &slot);
assert_eq!(tagged::pointer::<Arc<AtomicBool>>(raw), block);

shield.clear(0);
// SAFETY: the block is no longer reachable from a shared structure.
// Live readers either hold its address in their slot or have not yet
// passed `protect`.
unsafe { shield.retire(Retired::new(block.cast(), free_flag)) };
drop(shield);
domain.try_reclaim();
assert!(flag.load(Ordering::SeqCst));
```

### How to read the sources

Read in this order. Each file uses the words of the previous one. Comments
and rustdoc are in English and say why a particular `Ordering` is there.

1. `src/tagged.rs` — how one `usize` holds an address, a generation, and a mark.
2. `src/retired.rs` — what limbo is, and why `Retired` has no `Drop` that frees memory.
3. `src/reclaim/hazard.rs` — the most local scheme: a specific address is protected.
4. `src/stack/hazard.rs` — that scheme on a Treiber stack. `push` does not enter the domain. `pop` does.
5. `src/queue/hazard.rs` — the same ideas, but two nodes must be live at once, so a thread has two slots.
6. `src/map/hazard.rs` — Michael's list: a logical mark, then a physical unlink. Search helps finish someone else's removal.
7. `src/reclaim/epoch.rs`, `rcu.rs`, `deferred.rs` — the other three barriers. The stack, the queue, and the map on top of them repeat the same CAS loops. What changes is the wrapper around access to a node.
8. `src/atomic.rs` and `src/pad.rs` — the `std` / `loom` switch, and 128-byte padding of thread slots.
9. `tests/` — which invariants are pinned down by execution, and which remain a comment.

`src/atomic.rs` switches atomics, the fence, the mutex, and the spin between
`std` and `loom`. The domain handle is always `std::sync::Arc`, even under
`loom`. `loom::sync::Arc` must not be dropped from a TLS destructor: the
model has already finished by then, and touching its primitives fails the
test. `src/pad.rs` places each thread slot on its own 128-byte alignment so
neighboring atomics do not share a cache line. On a 64-byte x86 line that
is two lines, which also keeps a slot off its neighbor's prefetch.

### Two different problems

A lock-free list unlinks a node with one CAS. Two different questions remain
after that, and one scheme does not answer both.

#### When the memory may be destroyed

Thread A unlinks a node from the structure. Thread B has already loaded the
address and is still reading fields. A physical `drop` at that moment is a
use-after-free.

The order is:

1. The node is still in the structure. Both threads may read its fields.
2. A wins the CAS. The node is no longer reachable from new loads.
3. B, who entered earlier, is still inside the node.
4. A puts the node into limbo (`retire`) and does not call `drop` yet.
5. The scheme's barrier waits until B has definitely left.
6. Only then is the memory destroyed.

Hazard pointers, epoch, RCU, and deferred are responsible for step 5. Until
the barrier has passed, the node is deferred. A lost `Retired` does not free
memory on `Drop`: a leak is preferred to a second `free`.

#### When a CAS lies about "nothing changed"

ABA is about the CAS itself. It is a separate question from reclamation.

A stack, with addresses chosen so the story is easy to follow:

1. The head is node `0x1000`, generation 1. Its `next` is `0x2000`.
2. A thread loads the head and `next`, then stalls.
3. Other threads pop `0x1000` and later push that same node back. The head
   is `0x1000` again, but `next` is now `0x3000`.
4. The stalled CAS sees a familiar address and decides the link did not change.

If the word holds only the address, the CAS succeeds and swings the head to
`0x2000`. Node `0x3000` falls out of the stack, even though nobody popped it.
Reclamation is not involved: the node may have been reused without any
`free`, or the addresses may have matched for another reason. The generation
therefore sits in the same machine word as the address. At step 3 the head
would be `0x1000` with generation 2, and the CAS that expected generation 1
loses.

The generation increases on every successful CAS of the word. The thread
that lost rereads the head and takes the new generation.

#### What this means in the code

A reclamation scheme starts its work after the node has been unlinked. The
generation works at the moment of the CAS and keeps an old address from being
accepted as an unchanged link. On the stack the generation lives in the head
word: a node's `next` does not change after publication, and it is stored as
a bare `AtomicPtr`. On the queue and in a bucket list the CAS hits the link
itself, so the generation is packed into `next` as well.

A store into the structure is a `Release` CAS of the link, and it happens
before `retire`. A reader that observes the new link through `Acquire` sees
the node fields written before that CAS.

### Packing a word

The crate builds a pointer into a `usize` on a 64-bit platform.

```text
63            48 47                           1 0
+---------------+-----------------------------+-+
| generation u16| address, bits 1..=47        |M|
+---------------+-----------------------------+-+
```

| Bits | Functions | Meaning |
| --- | --- | --- |
| 0 | `is_marked`, `with_mark` | Harris/Michael logical-deletion mark. The stack and the queue leave it clear. A bucket list sets it. |
| 1..=47 | `pointer` | Address. The low bit of an address is always clear. |
| 48..=63 | `tag`, the `tag` argument of `pack` | Generation, 16 bits. |

`pack(ptr, tag, marked)` requires alignment of at least 2 and zero in the
top 16 bits of the address. A node `Box` is aligned more strictly than that:
it contains atomics. On x86_64 the top bits of a user address in the
canonical 48-bit form are either all zeros or all ones. The crate uses the
zero half for the generation. An address in the upper half is caught by a
`debug_assert` in `pack`.

A null pointer with a non-zero generation is a non-zero word:

```rust
let raw = epoch::tagged::pack::<u8>(std::ptr::null_mut(), 1, false);
assert_ne!(raw, 0);
assert!(epoch::tagged::pointer::<u8>(raw).is_null());
```

Emptiness is checked with `pointer(raw).is_null()`. Comparing the word with
zero answers a different question: "this word was never touched".

`with_mark` sets the low bit and keeps the address and the generation. After
2^16 successful CAS operations on the same address, `wrapping_add` repeats
the generation. That is a teaching limit: ABA on that address is possible
again after the wrap.

### Memory orderings

The correspondences the crate relies on. The names are `Ordering` variants.

| Place | Ordering | Why |
| --- | --- | --- |
| CAS of a structure link (`head`, `tail`, `next`) | success `Release`, failure `Acquire` | Success publishes fields written before the CAS. Failure rereads someone else's word. |
| Store of `next` in a node that is not published yet | `Relaxed` | The node is still private. A later `Release` CAS carries it out. |
| Load of an already published `next` | `Acquire` | Sees the store that the link's `Release` CAS published. |
| Claiming a domain slot | success `AcqRel` | The new owner sees that the previous owner stored "inactive" and released the slot. |
| Publishing a hazard address, a local epoch, or a deferred ticket | `Release`, then `fence(SeqCst)` | The scanner either sees the publication, or its unlink becomes visible to this thread's second load. |
| The second load of the word in `protect` and in `pin` | `Acquire` | It stands after the fence and after the slot publication. |
| Advancing the epoch, switching an RCU cohort | `SeqCst` | These stores share one total order with the reader's fence. |
| Deferred ticket `fetch_add` | `Release` | An unlink done earlier in this thread is visible to whoever `Acquire`-loaded the new ticket and only then reads the structure. |
| Clearing protection, writing "inactive" | `Release` | A scheme advance does not overtake this thread's reads and `retire`. |

The `SeqCst` fence in the reader/collector pair is there because one
`Release`/`Acquire` pair on two different atomics is not enough. The reader
writes a slot and then rereads the structure. The collector changes the
structure and then reads the slot. The fence keeps the reread of the
structure from rising above the slot publication, and it keeps the load of
the slot from rising above the unlink.

Under the `loom` feature the same calls go to the `loom` model. `std` atomics
and `loom` atomics must not be mixed in one scenario: they are different
types. The domain `Arc` stays `std::sync::Arc` in the model too.
`spin_loop` is `std::hint::spin_loop` normally and `loom::thread::yield_now`
under the feature, so a waiting RCU thread is a model switch point.

### Domain, slots, and limbo

Each collection owns its own domain. Domains are not merged into a
process-wide singleton, and tests do not share deferred nodes.

A domain has `MAX_THREADS` slots: 64 in a normal build and 4 under `loom`.
Walking 64 slots at every atomic inflates the model, so the limit for the
model is lower. A thread claims the first free slot with a CAS of `0 → 1`
(`AcqRel` on success) and remembers the index in thread-local storage.
Entering the same domain again from the same thread does not search for a
slot again. Registration is a `Vec` in TLS, one record per domain pointer
(`Arc::as_ptr`).

Slots are wrapped in `CachePadded`: alignment 128. On a 64-byte line that is
two lines, so neighboring threads do not write the same line.

`INACTIVE` is `usize::MAX`. For epoch and deferred, an occupied slot with
that label means "the thread is in the domain but not in a section". That
value does not hold reclamation back, neither in the minimum nor in the
epoch check.

Local limbo lives in TLS next to the slot index. `retire` puts a `Retired`
there: a raw pointer and a destruction function. When the thread exits, the
TLS destructor (in a build without `loom`) clears the slot and moves the
local list into the orphan list, under the domain mutex. Otherwise a dead
thread stays "active" forever and stops epoch, RCU, or deferred. Hazard
pointers do not depend on an epoch, but a slot occupied forever still
consumes the thread limit.

Under `loom` the TLS destructor does not touch the model's atomics.
`free_all_for_shutdown` drops the registration while the model is still
running the test. It uses `LocalKey::try_with` and `RefCell::try_borrow_mut`,
so a borrow that is already held does not panic the shutdown path.

A nested entry into the same domain on the same thread increments a depth.
Only the outer guard publishes and clears protection. Hazard-pointer nesting
shares the same two slots: the inner entry does not allocate a second pair
of pointers. The guard is not `Send`. `PhantomData<*const ()>` makes it
`!Send` and `!Sync`. It cannot move to another thread: the depth and the
slot are TLS state.

`Drop` of a collection requires that no other access is still in progress.
The live chain is then destroyed by a direct walk, without a barrier. Nodes
already deferred by this thread, and orphans, are taken by
`free_all_for_shutdown`, also without waiting for a barrier: exclusive
access is the proof that no reader remains.

The practical order is: `join` the threads that touched the collection, then
drop it. Limbo of a thread that is still alive sits in that thread's TLS.
`Drop` on another thread does not see that list. If that thread later
becomes the last holder of the domain `Arc`, nodes in the orphan list leak:
`Retired` does not free memory by itself. Hazard `abandon` moves the local
list into the orphan mutex and does not scan it. Epoch and deferred try to
reclaim orphans on the way out, and anything the barrier still holds stays
in the mutex until the domain is dropped. For RCU, limbo lives in the
domain's cohort queues from the start, not in TLS, so domain shutdown sees
those queues.

There is no mutex on the CAS path of the stack, the queue, or the list. The
mutex covers the orphan list and the RCU cohort queues. A thread that has
reached limbo collection may wait on that mutex. A poisoned mutex is read
through `into_inner`: a panic in another thread does not leave the list
unreachable.

More than `MAX_THREADS` simultaneous participants panic the domain with
`epoch: a domain admits at most N threads`.

### Hazard pointers

Maged Michael's scheme. A thread protects specific addresses it is reading
now, rather than "the whole epoch".

A thread slot holds:

- an occupied flag;
- two published pointers.

The stack needs one. The queue needs the head and the next node at the same
time. The map needs the current node and the previous node. The bucket head
is part of the table itself and is not written into a slot.

#### Protection protocol

`Shield::protect(i, atomic)`:

1. `Acquire`-load the word.
2. Store the bare address into slot `i`, with the generation and the mark
   stripped, using `Release`. The scanner compares block addresses. Two
   words with the same address and different generations are the same
   allocation.
3. Issue a `SeqCst` fence.
4. `Acquire`-load the word again.
5. If the word matches, it may be dereferenced. Otherwise the slot already
   points at an address that may no longer be in the structure, and the
   loop starts over.

`protect_ptr` writes an address this thread has already validated, and it
also issues the fence. The map uses it to move a node from the "current"
role to the "previous" role without opening a window where the address is
unprotected. Slot 0 is written before slot 1 moves on.

`clear` zeroes one slot. That is required before `retire` of one's own node:
otherwise the scan sees the thread's own slot and never destroys the block.

`Drop` of the outer shield zeroes both slots. An inner shield does not touch
the slots.

#### When memory is destroyed

`retire` puts the node on the local list. When that list holds at least
`2 * MAX_THREADS * 2` records (twice the number of published slots in the
domain; 256 in a normal build, 16 under `loom`), the thread scans:

1. A `SeqCst` fence, paired with the fence in `protect`.
2. A snapshot of every slot through `Acquire`.
3. Everything absent from the snapshot is a candidate for destruction. The
   rest stays in limbo.
4. Immediately before `free`, the address is checked again. `protect` may
   have published it after the snapshot. A false hit on a short failed
   `protect` attempt is safe: the node spends one more pass in the orphan list.

The threshold is the amortization from the paper. It is not a correctness
condition. A rare scan simply holds limbo longer. `try_reclaim` scans
without waiting for the threshold.

A long `protect` holds only the address written in the slot. Neighboring
nodes do not get stuck because of that thread. The local price of hazard
pointers is a slot write, a fence, and a second load for every dereferenced
address.

### Epoch

A global epoch counter `e`, and one slot per thread: whether it is occupied
and which epoch it published. A node retired in epoch `e` sits in bag
`e % 3` and is destroyed when the global number has reached `e + 2`.

Three bags are enough because the current epoch and the two previous ones
are alive at the same time. When the number becomes `e + 2`, the bag of
epoch `e` is free, and index `e % 3` may be reused.

#### Entry

`EpochDomain::pin`:

1. Claim a slot if the thread is not in the domain yet.
2. For the outer guard: load the global epoch, `Release`-store it into the
   slot, issue a `SeqCst` fence, and reload the global epoch. If the number
   changed, store the new one and repeat.
3. A nested `pin` only increments the depth. It does not publish the epoch again.

While the outer guard is alive, the slot holds the number read on entry.
`Drop` of the outer guard writes `INACTIVE` with `Release` and tries to
advance the epoch.

#### Why reclamation waits two epochs

An advance loads the global number, issues a `SeqCst` fence, and looks at
the slots. An occupied slot whose number differs from the current global
epoch and from `INACTIVE` forbids the step. Otherwise a `SeqCst` CAS
increases the number by one (`wrapping_add`).

While a guard is published in epoch `e`, the number does not move past
`e + 1`. The step `e → e + 1` lets that guard through: its number matches
the global one. The step `e + 1 → e + 2` stops on it. References taken
inside a pin of epoch `e` have therefore been released by the time the
global epoch is `e + 2`. Nodes retired in `e` are destroyed at that moment.
The check in the code is `global.saturating_sub(epoch) >= 2`.

A single thread, as numbers:

| Step | Global epoch | A node retired in 0 |
| --- | --- | --- |
| pin, retire into bag `0 % 3` | 0 | sits in the bag |
| unpin, first successful step | 1 | `1 - 0 < 2`, still alive |
| next step, no other pin | 2 | `2 - 0 >= 2`, destroyed |

`try_reclaim` tries to take two steps. The second one is often missing: a
thread that still holds a pin in epoch `e` is, after the first step, strictly
behind the global number. A node retired just now survives this call. Two
steps are enough to finish a bag that was waiting for exactly that window.

If a thread has not collected limbo for a long time and a different epoch
arrives in slot `e % 3`, the old nodes are moved to overflow together with
their number. They are not mixed into the new bag, and they are destroyed
by the same `+ 2` rule.

A long pin holds reclamation back for everyone who is at most one epoch
behind. That is the price of the scheme: reading is cheap (one publication
per operation), and a thread that falls asleep inside a pin stops the counter.

### RCU

Userspace RCU on two reader counters. There is no epoch number here.

Domain state:

- the index of the current cohort, the low bit of the `current` atomic;
- two reader counters;
- two queues of deferred nodes, one per cohort;
- a flag "someone is already running a grace period";
- a slot table, so the thread limit is enforced and a section is cleared on exit.

#### Reader entry

`RcuDomain::lock`:

1. Load the cohort index.
2. `fetch_add(1)` on its counter with `AcqRel`.
3. A `SeqCst` fence.
4. If the index is unchanged, the section has begun.
5. Otherwise roll the counter back with `fetch_sub` and repeat. The wait
   calls `spin_loop`.

A nested guard does not take the cohort again: the depth grows, and only
the outer entry incremented the counter. `Drop` of the outer guard
decrements the counter first and the depth second. The reverse order would
open a window in which this thread's `retire` already sees depth zero while
the counter still includes the thread itself.

#### Reclamation

`retire` refuses to run inside a read-side section of this thread and panics
with a message about waiting on one's own counter. The collections therefore
close the guard with a block and call `retire` outside it. On the stack and
the queue that is a separate block in `pop` and `dequeue`.

Then:

1. The node is pushed onto the current cohort's queue.
2. The thread tries to take the synchronization flag with a CAS of `0 → 1`.
   If the flag is already taken, the node simply waits for the next pass.
3. The winner loads the index, switches it to the other cohort with a
   `SeqCst` store, and spins until the previous counter is zero.
4. The previous cohort's queue is taken whole and destroyed.
5. The flag is cleared with `Release`.

A reader that incremented the old counter and has not yet rechecked the
index either rolls back at step 5 of entry, or is counted in the wait. A
reader that successfully entered the new cohort did so after the switch,
which is after `retire` and after the unlink: the unlinked node is no
longer reached by that reader.

The thread that took the grace period spins until readers of the old cohort
leave. If another thread already holds the flag, `retire` returns at once
and the node waits in the cohort queue for the next pass. Epoch, hazard
pointers, and deferred do not wait on a foreign thread inside `retire` in
that way. `enqueue` defers no nodes. The wait on the counter is done by
`dequeue`, because that is the operation that has something to put in limbo.

RCU limbo lives in the domain queues, under a mutex, from the moment of
`retire`. Shutdown of the domain sees both queues. In debug builds
`free_all_for_shutdown` also asserts that both reader counters are zero:
exclusive `Drop` is the reason those counters are empty.

### Deferred

There is no epoch number and there are no three bags. There is a monotonic
ticket.

`retire` does `fetch_add(1)` with `Release` and remembers the old value as
the node's stamp. On entry to a section the thread publishes the latest
ticket it has seen: an `Acquire` load of the counter, a `Release` store into
the slot, a `SeqCst` fence. It then reads the structure. On exit it writes
`INACTIVE` and tries to collect limbo.

The destroyed prefix is every record whose stamp is strictly less than the
minimum over occupied slots. Free slots do not take part in the minimum. An
inactive occupied slot stores `usize::MAX` and does not lower the minimum.
If there are no sections at all, the minimum stays `usize::MAX`, and any
stamp that has not wrapped `usize` is less than it.

The strict comparison is part of the protocol.

- A reader loaded ticket `5` and published `5`. The next `retire` receives
  stamp `5`. `5 < 5` is false, so the node stays. That reader may have
  entered before it saw the unlink.
- A `retire` with stamp `5` happened earlier, the ticket became `6`, and the
  store was `Release`. A reader `Acquire`-loaded `6`, published `6`, and only
  then read the structure: it sees the unlink. Stamp `5 < 6`, so the node
  may be destroyed even while that section is still running.

`retire` from inside a section is allowed. One's own published label is not
older than the stamp the section itself just took, so one's own node is not
destroyed before the guard's `Drop`. That is how deferred differs from RCU,
where the same call panics.

A long section holds every node whose stamp is greater than or equal to its
ticket. A thread that falls asleep inside `enter` stops collection the same
way a long pin does.

Ticket overflow is a theoretical limit at `usize::MAX` steps. The 16-bit
pointer generation runs out earlier. The global epoch uses `wrapping_add`
and `saturating_sub`, so it has the same kind of theoretical wrap.

### Comparing the schemes

| | What the reader publishes | When `retire` is called | When memory is destroyed | Cost of a read | What a sleeping thread holds |
| --- | --- | --- | --- | --- | --- |
| Hazard pointers | Specific addresses in two slots | Immediately, into the local list. The thread's own slot is cleared first | The address is in no slot | Per address: a load, a store, a fence, a second load | Only the addresses it wrote |
| Epoch | The global epoch number | Inside the pin, into bag `e % 3` | Global epoch ≥ `e + 2` | One publication per operation | Advance past `e + 1` |
| RCU | Plus one on the cohort counter | Only outside the read-side section | The old counter hit zero after the cohort switch | `fetch_add`, a fence, a recheck of the index | `retire` spins until that section ends |
| Deferred | The ticket it has seen | Allowed inside the section | Stamp strictly below the minimum active label | A ticket store and a fence | Every stamp ≥ its ticket |

All four share this: a node first becomes unreachable from the structure,
and only then enters limbo. They share a limit too: a section that does not
end postpones destruction. Hazard pointers postpone it for specific
addresses. The other three postpone it in batches.

### Treiber stack

The head is one packed word. An empty stack is a null address and some
generation; the address is what gets checked, not equality of the word with
zero.

A node holds a canary, an `Option<T>`, and an atomic bare pointer `next`.
The canary is checked with an ordinary `assert` at destruction and is then
overwritten with zero. A second `free` fails in release builds too. It is a
trap on an invariant. It is not a defense against corruption. The stack
canary is `0xE4C0_FFEE_E4C0_FFEE`.

`push`:

1. Allocate a node. Only the current thread can see it.
2. Load the head with `Acquire`.
3. Store its address into `next` with `Relaxed`.
4. CAS the head to the new node with a generation one higher. Success is `Release`.

No foreign node is dereferenced, so neither a hazard slot, nor a pin, nor a
cohort, nor a ticket is required. A thread that only pushes does not occupy
a domain slot at all.

`pop`:

1. Enter the scheme.
2. Load the head. For hazard pointers this is `protect` of slot 0. For the
   other schemes the load happens inside a section that is already open.
3. A null address means the stack is empty.
4. Load `next`.
5. CAS the head onto that `next` with a new generation.
6. Take `T` out of the node.
7. For hazard pointers, clear the slot.
8. `retire`. For RCU, step 8 stands after the guard has been closed. For
   epoch and deferred the guard is still alive at that moment and itself
   keeps the node from being destroyed.

The value is returned to the caller. `Drop` of `T` happens at the caller,
not at the moment the node is freed: by then the `Option` inside the node
is already empty.

Other threads observe the operation through the successful CAS of the head.

`Drop` of the stack walks the chain from the head and destroys nodes
immediately, then finishes the domain's limbo.

Types: `HazardStack<T>`, `EpochStack<T>`, `RcuStack<T>`, `DeferredStack<T>`.
The `Send` bound on `T` is the same as for `Mutex`: ownership of the value
moves between threads, and they share only `&Self`.

### Michael–Scott queue

The queue starts with a dummy node. In an empty queue `head` and `tail`
point at it, its `next` is empty, and it holds no value. Data lives in the
nodes that follow the head. `next` is a packed word, because a CAS changes
it after publication. The queue canary is `0xC0FF_EE00_C0FF_EE00`.

```text
enqueue(1), enqueue(2):

head                                    tail
  |                                       |
  v                                       v
+------+     +------+     +------+
| dummy|---->|  1   |---->|  2   |----> null
+------+     +------+     +------+

dequeue takes 1 and retires the dummy.
The new head is the former node that held 1, now without a value.

head                 tail
  |                    |
  v                    v
+------+     +------+
| dummy|---->|  2   |----> null
| (was 1)    +------+
+------+
```

What goes to limbo is the old head, not the node whose value was just taken.
That node becomes the new dummy head. A neighbor may still be reading `next`
of the old head, so it cannot be destroyed immediately.

`enqueue`:

1. Allocate a node with the value and an empty `next`.
2. Enter the scheme. The tail is about to be dereferenced.
3. Load `tail`. Hazard pointers protect it with slot 0.
4. Load `tail.next`.
5. If the tail changed in the meantime, start over.
6. If `next` is empty, CAS the new node onto it. Success publishes the value.
   Then try to swing `tail` onto the new node. Losing that second CAS is
   safe: the node is already in the list, and the next attempt will catch
   the tail up.
7. If `next` is not empty, the tail has fallen behind. A CAS swings it one
   node forward, and the loop looks for the end again. There is no jump over
   a node: one iteration moves the tail by one link.

`dequeue`:

1. Enter the scheme.
2. Load the head. Hazard pointers: slot 0.
3. Load `head.next`. Hazard pointers: slot 1, because the value lives in
   that node and has to be held too.
4. If the head changed, start over.
5. If the head pointer matches the tail pointer:
   - `next` empty — the queue is empty;
   - otherwise the tail has fallen behind, it is swung forward, and the loop repeats.
6. If the head and the tail differ and `next` is still empty, the snapshot
   is stale. A null pointer is not dereferenced. The loop loads the head again.
7. A CAS swings the head onto `next`.
8. The value is taken from the new head node. The old head goes to `retire`.
   Hazard pointers clear both slots first. For RCU the guard is already closed.

Other threads observe `enqueue` through the successful CAS of the `next`
link, not through the tail swing. They observe `dequeue` through the
successful CAS of the head.

`Drop` walks the chain from the head, including the dummy node, and then
shuts the domain down. `head` and `tail` are zeroed so a second walk goes
nowhere.

Types: `HazardQueue<T>`, `EpochQueue<T>`, `RcuQueue<T>`, `DeferredQueue<T>`.
The bound on `T` is `Send`, as on the stack.

### Hash map

`HazardMap<K, V>` and `EpochMap<K, V>`. The bucket count is given to `new`
and never changes: zero buckets panics. Replacing the bucket array is a
separate problem, because the pointer to the array itself would also have
to be protected.

A bucket is chosen as `DefaultHasher(key) % buckets`. There is no per-instance
random seed. The key is `Hash + Ord`. Order is what the list needs. The hash
only chooses the bucket.

Each bucket is the head of a sorted Michael list. The head lives in the
table body and does not go to limbo. A node stores the key, the value, and
a packed `next`. After publication the key and the value do not change: a
repeated `insert` does not replace the old value. The map canary is
`0xA11C_E5A1_1CE5_A110`.

Deletion has two steps.

1. Logical: a CAS sets the low bit of `next`. From that moment a search must
   not return the key. The generation and the address in this CAS stay the same.
2. Physical: a CAS on the predecessor swings the link onto the node that
   follows the marked one. The new word carries the next generation.

`remove` returns `true` after a successful mark, even if the physical CAS
lost. The unlink and the `retire` are then done by a search that runs into
the mark. Only the thread whose unlink CAS won puts the node into limbo, so
there is no second `retire`. A marked node that is still hanging in the
chain is taken by `Drop` of the table: the walk follows live links, and
those nodes are still there.

Search walks keys in ascending order and helps unlink marked nodes along the way.

- The key is found — `Found`, with a pointer to the predecessor's link.
- A greater key or the end of the list is met — `Missing`: that is where an insert goes.
- The predecessor no longer points at the current node — the walk is stale,
  and the search restarts from the bucket head.
- For hazard pointers, slot 1 holds the current node before its fields are
  read. Before stepping forward, `protect_ptr` writes that same node into
  slot 0. The order matters: the current address must not be released and
  only afterwards protected as the previous node.

`insert`:

- the key is already present — the new node was never published, it is
  destroyed immediately, the method returns `false`, and the old value stays;
- the key is absent — the new node's `next` points at the insertion point,
  the predecessor's CAS publishes the node, and the method returns `true`;
- the CAS loses — the node stays local, and the search for a place starts over.

`get` requires `V: Clone`. The method copies the value while the scheme
holds the node, and it does not hand a reference out. If the node is marked
before or after the clone, the search repeats and that copy is not returned.
By the time the method returns, the guard may be destroyed: the caller has
its own copy.

`Send` and `Sync` for the map require `Send + Sync` on both the key and the
value. Several threads compare the key, and the value is cloned under
protection. On the stack and the queue the value is not shown to anyone. It
is handed to one thread, so `Send` is enough there.

There is no RCU map and no deferred map in the crate. Their walk would match
epoch: one section for the whole operation, with no per-node slots.

`Drop` of the key and the value happens at the moment the node is physically
freed. That can be inside `retire`, still inside the deleting thread's
section. That `Drop` must not enter the same table again. A nested
hazard-pointer entry writes the same two slots and wipes the protection of
the outer walk.

### Public API

Collections are exported from the crate root.

| Type | Constructors | Operations | Bounds |
| --- | --- | --- | --- |
| `HazardStack<T>`, `EpochStack<T>`, `RcuStack<T>`, `DeferredStack<T>` | `new`, also `Default` | `push`, `pop -> Option<T>` | `T: Send` for the type to be `Send + Sync` |
| `HazardQueue<T>`, `EpochQueue<T>`, `RcuQueue<T>`, `DeferredQueue<T>` | `new`, also `Default` | `enqueue`, `dequeue -> Option<T>` | `T: Send` |
| `HazardMap<K, V>`, `EpochMap<K, V>` | `new(buckets)` | `insert -> bool`, `remove -> bool`, `get -> Option<V>` | `K: Hash + Ord + Send + Sync`, `V: Send + Sync`, and `V: Clone` for `get` |

`insert` returns `false` when the key was already present. `remove` returns
`false` when the key was absent. `remove` does not return the value: the
value is destroyed together with the node.

The `reclaim` module exposes the domains. Every `new` returns `Arc<Domain>`.

| Type | Entry | Defer a node | Forced collection attempt |
| --- | --- | --- | --- |
| `HazardDomain`, `Shield` | `HazardDomain::enter` | `Shield::retire` | `try_reclaim` |
| `EpochDomain`, `EpochGuard` | `EpochDomain::pin` | `EpochGuard::retire` | `try_reclaim` |
| `RcuDomain`, `RcuGuard` | `RcuDomain::lock` | `RcuDomain::retire` | `try_reclaim` |
| `DeferredDomain`, `DeferredGuard` | `DeferredDomain::enter` | `retire` on the domain and on the guard | `try_reclaim` |

The hazard-pointer shield also has `protect`, `protect_ptr`, and `clear`.
The slot index is 0 or 1. Any other index panics.

`Retired::new` is unsafe. The pointer must be a block that the given
destruction function understands. The function is called once, and nobody
uses the pointer after that.

`tagged` exports `pack`, `pointer`, `tag`, `is_marked`, and `with_mark`.

`reclaim::MAX_THREADS` is 64, or 4 when the crate is built with `--features loom`.

Guards and shields borrow the domain and carry `PhantomData<*const ()>`, so
they are `!Send` and `!Sync`. Move the collection, not the guard.

### Unsafe contract

A public collection keeps these rules inside itself. A direct call of a
domain moves the last items onto the caller.

- The pointer in `Retired` was allocated as the type that `drop_fn` destroys.
- `drop_fn` is called exactly once and takes ownership of the value.
- By the time of `retire` the node is unreachable from the structure. Threads
  that may still read its fields sit in the scheme's protection: a slot, a
  pin, a cohort, or a published ticket.
- The `Release` CAS that published the node stands earlier in program order than `retire`.
- For RCU, `retire` is called at read-side depth zero on this thread.
- For hazard pointers, a slot that still stores this node's address is cleared
  before `retire` when that slot belongs to the same thread.
- `Drop` of a collection does not overlap other threads' calls of its methods.
- The canary in the node is still the module constant at destruction time.
  Otherwise this is a second `free`, and `assert` stops the thread.

`unsafe impl Send/Sync` stands on the collections, not on the domains. A
domain contains only addresses and counters. A collection additionally
promises that `T` (and the key and the value of a map) may be sent to another
thread under the bounds written on the `impl`.

### Panics and asserts

These strings are part of the crate. They are in English, like the comments.

| Where | Message | When |
| --- | --- | --- |
| `claim_slot` in every domain | `epoch: a domain admits at most {MAX_THREADS} threads` | Every slot is occupied. |
| `Shield::protect`, `protect_ptr`, `clear` | `a thread has only two hazard slots` | The index is not 0 or 1. |
| `RcuDomain::retire` | `RCU: retire inside a read-side critical section would wait on this thread's own reader count` | `retire` while this thread's read-side depth is non-zero. |
| `with_local` | `thread has not entered the … domain` | `retire` or another TLS operation without a prior `enter` / `pin` / `lock` on this thread. |
| `HazardMap::new`, `EpochMap::new` | `at least one bucket is required` | `buckets == 0`. |
| `drop_node` | `double free of a stack/queue/map node` | The canary is not the module constant. Stays in release builds. |
| `tagged::pack` | alignment and top-16-bits messages | `debug_assert` only. |
| crate root of `tagged` | `epoch requires 64-bit pointers` | Compile-time `assert` on `usize::BITS`. |

`debug_assert_eq!(depth, 0)` on the outer guard's `Drop` checks that the
depth accounting matches the outer flag. RCU shutdown `debug_assert`s that
both reader counters are zero.

### Checks

```sh
cargo test
cargo test --features loom
cargo miri test
```

`cargo test` runs `std` atomics.

`tests/reclaim.rs` hits the domains directly. A "bomb" is a block whose
`Drop` raises a flag. While the flag is clear, there has been no physical
free.

- A protected hazard pointer is not destroyed. After `clear` it is.
- An epoch pin and a deferred section hold the node until exit, including
  when another thread performs `retire`.
- RCU does not destroy a node while a reader holds the cohort: another
  thread spins in `retire`, the flag stays clear, and after `drop(guard)`
  the node is freed.
- A hazard-pointer scan sees another thread's slot.

`tests/collections.rs` checks three things for every implementation.

- The stack returns LIFO, the queue returns FIFO, and the map matches a mix
  of `insert` / `get` / `remove`. A repeated `insert` does not overwrite the value.
- `Drop` of a value happens once. For the stack and the queue the count is
  on values that were taken out: the node is empty by then. For the map a
  failed `insert` drops the new value immediately, a successful `remove` may
  delay `Drop` until the physical free, and whatever remains is finished by
  `Drop` of the map.
- Several threads neither lose nor duplicate elements. A normal run is 4
  threads and 200 operations. Under Miri it is 2 threads and 16 operations,
  because the interpreter is expensive.

`cargo test --features loom` replaces the crate's atomics with the model's
atomics. The integration tests `reclaim.rs` and `collections.rs` are disabled
in that build. `tests/loom.rs` is built instead. The pointer-packing unit
tests stay. The model is short: `preemption_bound = 2`, at most 8000
branches, two threads, and two or three operations. It checks that a stack
does not lose a pair of elements (`HazardStack`, `EpochStack`, `RcuStack`,
`DeferredStack`), that a single-producer `EpochQueue` keeps FIFO order, and
that epoch does not destroy a block while another thread holds a pin. That
is an exhaustive search of a small scenario. It is not coverage of every
path of the four schemes.

`cargo miri test` runs the functional suite without the `loom` feature.
Miri and loom are not run together: the model has its own atomics.

Canaries and the double-free `assert` stay in release builds of the tests.
The `debug_assert` in `pack` checks alignment and the top bits of the
address only in debug.

### Repository map

```text
src/lib.rs                 crate root: re-exports of the collections
src/atomic.rs              std or loom
src/pad.rs                 a slot on its own cache line
src/tagged.rs              address, generation, mark
src/retired.rs             a limbo record
src/reclaim/mod.rs         thread limit and INACTIVE
src/reclaim/hazard.rs      hazard pointers
src/reclaim/epoch.rs       three bags
src/reclaim/rcu.rs         two cohorts
src/reclaim/deferred.rs    tickets
src/stack/                 Treiber stack, one file per scheme
src/queue/                 Michael–Scott queue, one file per scheme
src/map/                   hash map, hazard and epoch
examples/stack.rs          two threads, hazard stack
examples/queue.rs          FIFO, hazard queue
tests/reclaim.rs           barriers, exercised with bombs
tests/collections.rs       order, Drop, races
tests/loom.rs              small models
```

The `loom` feature is optional and enables the `loom` 0.7 dependency. The
crate has no other dependencies.

### Limits

- At most 64 threads per domain, and at most 4 in a `loom` build. An extra
  thread panics on entry to the domain.
- The hash map does not grow. The hash is `DefaultHasher` with no per-instance
  seed. The bucket is the remainder of division.
- The pointer generation is 16 bits and wraps. The deferred ticket and the
  epoch number wrap only together with `usize`.
- A long pin, read-side section, or deferred section holds reclamation back.
  Hazard pointers hold back only the published addresses.
- `retire` inside an RCU read-side section of the same thread panics. In the
  collections that call stands after the guard.
- A nested entry into a hazard domain shares two slots with the outer entry.
  `Drop` of a map value must not touch that same map from the inside.
- The orphan list and the RCU queues are taken under a `Mutex`. The CAS of
  the structure does not take it. Limbo collection does.
- `get` returns a clone. The API does not hand out a reference into reclaimable memory.
- A repeated `insert` does not update the value.
- `remove` does not return the old value. `true` means logical deletion;
  another search may finish the physical unlink.
- A live collection must not be destroyed while another thread has not
  finished using it and has not given up its TLS. Otherwise its limbo can
  outlive the domain and leak, instead of being freed.
- Pointers are 64-bit only. The top 16 bits of an address must be zero.
- The crate's job is to be read, algorithm next to atomic. In the Rust
  ecosystem the neighboring role is `crossbeam-epoch`: it has no hard thread
  ceiling and a different amount of engineering.

### Glossary

| Word | What it is in this crate |
| --- | --- |
| ABA | A CAS sees the previous address and decides the link did not change, although the node was removed and inserted again. Closed by the generation in the same word. |
| Bucket | The head of one sorted list. Lives in the table body. |
| Canary | A constant at the start of a node. A second `free` clears it and fails the next `assert`. |
| Cohort | One of the two RCU reader groups, with its own counter and its own limbo queue. |
| Grace period | The interval after which readers that saw the old structure have definitely left. For RCU that is the previous counter hitting zero. |
| Limbo | Nodes already removed from the structure and not yet destroyed. |
| Mark | The low bit of a link. When set, the key is logically deleted. |
| Orphan | Limbo of a thread that has already exited. It sits in the domain until some scan or the collection's `Drop` takes it. |
| Pin | Entry into an epoch: the slot publishes the global number before the structure is read. |
| Retire | Put an unlinked node into limbo. |
| Shield | The hazard-pointer window on the current thread. While it is alive, the thread may write its two slots. |
| Slot | One thread's record in the domain: occupancy and the published protection. |
| Tag / generation | The top 16 bits of a word. Grows on a successful CAS. |
| Ticket | The monotonic deferred number. `retire` takes the next one. A section publishes the latest one it has seen. |
| Dummy node | The queue head, with no value. `dequeue` unlinks it and reads the data from the next node. |

### References

The implementations are teaching versions and shorter than the papers, but
the algorithm names match these texts.

- R. K. Treiber. *Systems Programming: Coping with Parallelism.* IBM Technical Report RJ 5118, 1986. A stack on a CAS of the head.
- M. M. Michael, M. L. Scott. *Simple, Fast, and Practical Non-Blocking and Blocking Concurrent Queue Algorithms.* PODC 1996. A queue with a dummy head and a tail that is swung forward.
- T. L. Harris. *A Pragmatic Implementation of Non-Blocking Linked-Lists.* DISC 2001. The low bit as logical deletion.
- M. M. Michael. *High Performance Dynamic Lock-Free Hash Tables and List-Based Sets.* SPAA 2002. A list with a mark and an unlink, including helping someone else's removal during search.
- M. M. Michael. *Hazard Pointers: Safe Memory Reclamation for Lock-Free Objects.* IEEE TPDS, 2004. The protocol "publish the address, fence, reread" and the threshold `2 * number of slots`.
- K. Fraser. *Practical Lock-Freedom.* PhD thesis, University of Cambridge, 2004. Epoch-based reclamation. Three bags `e % 3` and the wait for `e + 2` are the same scheme `crossbeam-epoch` uses.
- P. E. McKenney, userspace RCU in the liburcu line (Mathieu Desnoyers). Two counters and an index switch — this variant, rather than quiescent-state RCU, where the reader has no atomic counter.
- The deferred tickets in `reclaim/deferred.rs` are left as the direct variant: one counter and a minimum over slots. By role this is the closest relative of epoch. Industrial schemes with intervals (hazard eras and interval-based reclamation) are built more carefully, and this crate does not repeat them.

## Русский

Учебный крейт с четырьмя схемами безопасного освобождения памяти и lock-free
структурами поверх них. Код написан так, чтобы алгоритм читался рядом с
атомиком: у каждой коллекции свой файл, свой цикл CAS и свой комментарий про
ordering. Комментарии и rustdoc в исходниках — на английском.

Публичные методы коллекций безопасны. `unsafe` остаётся внутри схем и
структур. Инварианты записаны в rustdoc и здесь. Крейт рассчитан на чтение
реализации. Для боевого кода берут `crossbeam-epoch` и промышленные очереди:
здесь фиксированное число потоков, нет роста хеш-таблицы, 16-битное
поколение указателя заворачивается, а путь освобождения местами берёт мьютекс.

Лицензия MIT, edition 2024, `publish = false`. Указатели 64-битные: это
проверяется ещё при компиляции (`usize::BITS == 64`). Единственная
необязательная зависимость — `loom` 0.7 за фичей `loom`. Других зависимостей нет.

### Содержание

- [Быстрый старт](#быстрый-старт)
- [Как читать исходники](#как-читать-исходники)
- [Две разные проблемы](#две-разные-проблемы)
- [Упаковка слова](#упаковка-слова)
- [Барьеры памяти](#барьеры-памяти)
- [Домен, слоты и limbo](#домен-слоты-и-limbo)
- [Hazard pointers](#hazard-pointers-1)
- [Epoch](#схема-epoch)
- [RCU](#схема-rcu)
- [Deferred](#схема-deferred)
- [Сравнение схем](#сравнение-схем)
- [Стек Трайбера](#стек-трайбера)
- [Очередь Michael–Scott](#очередь-michaelscott)
- [Хеш-таблица](#хеш-таблица)
- [Публичный API](#публичный-api)
- [Контракт unsafe](#контракт-unsafe)
- [Паники и assert](#паники-и-assert)
- [Проверки](#проверки)
- [Карта репозитория](#карта-репозитория)
- [Ограничения](#ограничения)
- [Словарь](#словарь)
- [Ориентиры](#ориентиры)

### Быстрый старт

Стек, очередь и таблица не прячутся за общим trait. У каждой схемы свой тип,
чтобы цикл CAS и комментарий к барьеру лежали в одном файле.

```rust
use epoch::{EpochMap, EpochQueue, HazardStack};

let stack = HazardStack::new();
stack.push(1);
stack.push(2);
assert_eq!(stack.pop(), Some(2));

let queue = EpochQueue::new();
queue.enqueue(1);
queue.enqueue(2);
assert_eq!(queue.dequeue(), Some(1));

let map = EpochMap::new(16); // число бакетов фиксировано
assert!(map.insert(1, 10));
assert_eq!(map.get(&1), Some(10)); // значение клонируется
assert!(map.remove(&1));
```

Коллекции `Send + Sync` при границах из таблицы API и делятся между потоками
как `Arc<HazardStack<_>>`. Примеры в `examples/` гоняют стек и очередь на
hazard pointers:

```sh
cargo run --example stack   # печатает: stack: [1, 2]
cargo run --example queue   # печатает: queue: Some(1), Some(2)
```

Схемы можно звать и без коллекции. Домен живёт в `Arc`, guard привязан к
потоку и к жизни ссылки на домен. `retire` небезопасен: вызывающий обязан
уже вынуть блок из всех разделяемых структур.

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use epoch::reclaim::{HazardDomain, Retired};
use epoch::tagged;

unsafe fn free_flag(ptr: *mut u8) {
    let flag = unsafe { Box::from_raw(ptr.cast::<Arc<AtomicBool>>()) };
    flag.store(true, Ordering::SeqCst);
}

let domain = HazardDomain::new();
let flag = Arc::new(AtomicBool::new(false));
let block = Box::into_raw(Box::new(Arc::clone(&flag)));
let slot = AtomicUsize::new(tagged::pack(block, 1, false));
let shield = HazardDomain::enter(&domain);

// Слово вернулось тем же, значит адрес опубликован в слоте и его можно читать.
let raw = shield.protect(0, &slot);
assert_eq!(tagged::pointer::<Arc<AtomicBool>>(raw), block);

shield.clear(0);
// SAFETY: блок уже недостижим из разделяемых структур. Живые читатели
// держат его адрес в своём слоте либо ещё не прошли protect.
unsafe { shield.retire(Retired::new(block.cast(), free_flag)) };
drop(shield);
domain.try_reclaim();
assert!(flag.load(Ordering::SeqCst));
```

### Как читать исходники

Имеет смысл идти в таком порядке. Каждый следующий файл пользуется словами
предыдущего. Комментарии рядом с `Ordering` объясняют, зачем выбран именно
этот барьер.

1. `src/tagged.rs` — как в одном `usize` лежат адрес, поколение и метка.
2. `src/retired.rs` — что такое limbo и почему у `Retired` нет `Drop`, который освобождает память.
3. `src/reclaim/hazard.rs` — самая локальная схема: защищается конкретный адрес.
4. `src/stack/hazard.rs` — схема на стеке Трайбера. `push` в домен не входит, `pop` входит.
5. `src/queue/hazard.rs` — те же идеи, но живыми одновременно должны быть два узла, поэтому у потока два слота.
6. `src/map/hazard.rs` — список Майкла: логическая пометка, затем физическая вырезка, поиск помогает доделать чужое удаление.
7. `src/reclaim/epoch.rs`, `rcu.rs`, `deferred.rs` — остальные три барьера. Стек, очередь и таблица на них повторяют те же CAS, меняется только то, чем обёрнут доступ к узлу.
8. `src/atomic.rs` и `src/pad.rs` — переключение `std` / `loom` и выравнивание слота на 128 байт.
9. `tests/` — какие инварианты закреплены исполнением, а какие остаются комментарием.

`src/atomic.rs` переключает атомики, барьер, мьютекс и spin между `std` и
`loom`. Ручка домена всегда `std::sync::Arc`, в том числе под `loom`.
`loom::sync::Arc` нельзя ронять в деструкторе TLS: модель к тому моменту уже
завершена, и обращение к её примитивам завершает тест ошибкой. `src/pad.rs`
разносит слоты потоков на 128 байт, чтобы соседние атомики не делили
кеш-линейку. На линейке в 64 байта это две линии: слот не слипается с соседом
из-за предвыборки.

### Две разные проблемы

Lock-free список вынимает узел одним CAS. После этого остаются два разных
вопроса, и одна схема на оба не отвечает.

#### Когда можно уничтожить память

Поток A вынул узел из структуры. Поток B уже прочитал адрес и ещё читает
поля. Физический `drop` в этот момент — use-after-free.

Порядок такой:

1. Узел ещё в структуре, оба потока могут читать его поля.
2. A выигрывает CAS и узел перестаёт быть достижимым из новых чтений.
3. B, который вошёл раньше, всё ещё стоит внутри узла.
4. A кладёт узел в limbo (`retire`) и пока не вызывает `drop`.
5. Барьер схемы дожидается, пока B точно вышел.
6. Только теперь память уничтожается.

За шаг 5 отвечают hazard pointers, epoch, RCU и deferred. Пока барьер не
пройден, узел числится отложенным. Потерянный `Retired` при `Drop` память не
освобождает: лучше утечка, чем повторный `free`.

#### Когда CAS врёт про «ничего не менялось»

ABA — про сам CAS, а не про освобождение.

Стек, адреса для наглядности:

1. Голова — узел `0x1000`, поколение 1, его `next` — `0x2000`.
2. Поток читает голову и `next`, затем отстаёт.
3. Другие потоки снимают `0x1000` и позже ставят тот же узел обратно.
   Теперь голова снова `0x1000`, но `next` уже `0x3000`.
4. Отставший CAS видит знакомый адрес и считает, что звено не менялось.

Если в слове лежит только адрес, CAS успешен и ставит голову на `0x2000`.
Узел `0x3000` выпадает из стека, хотя его никто не снимал. Освобождение
памяти тут ни при чём: узел могли использовать повторно, вообще не вызывая
`free`, либо адреса совпали по другой причине. Поэтому поколение сидит в том
же машинном слове, что и адрес. В шаге 3 голова стала бы `0x1000` с
поколением 2, и CAS, ожидавший поколение 1, проигрывает.

Поколение увеличивается на каждом успешном CAS слова. Проигравший попытку
перечитывает голову и берёт уже новое поколение.

#### Что из этого следует для кода

Схема освобождения начинает работать после того, как узел вынут. Поколение
работает в момент CAS и не даёт принять старый адрес за неизменное звено.
В стеке поколение живёт в слове головы: `next` узла после публикации не
меняется и хранится голым `AtomicPtr`. В очереди и в списке бакета CAS бьёт
по самому звену, поэтому поколение упаковано в `next` тоже.

Запись в структуру делает `Release`-CAS звена до `retire`. Читатель, увидевший
новое звено через `Acquire`, наблюдает поля узла, записанные до этого CAS.

### Упаковка слова

Крейт собирает указатель в `usize` на 64-битной платформе.

```text
63            48 47                           1 0
+---------------+-----------------------------+---+
| поколение u16 | адрес, биты 1..=47          | M |
+---------------+-----------------------------+---+
```

| Биты | Функции | Смысл |
| --- | --- | --- |
| 0 | `is_marked`, `with_mark` | Метка логического удаления Harris/Michael. Стек и очередь оставляют её нулём. Её ставит список бакета. |
| 1..=47 | `pointer` | Адрес. Младший бит адреса всегда нулевой. |
| 48..=63 | `tag`, поле `tag` у `pack` | Поколение, 16 бит. |

`pack(ptr, tag, marked)` требует выравнивания минимум на 2 и нулевых старших
16 бит адреса. `Box` узла выровнен сильнее: внутри лежат атомики. На x86_64
старшие биты пользовательского адреса в канонической 48-битной форме либо
все нули, либо все единицы. Крейт занимает нулевую половину под поколение.
Попадание адреса в верхнюю половину ловит `debug_assert` в `pack`.

Пустой указатель с ненулевым поколением — ненулевое слово:

```rust
let raw = epoch::tagged::pack::<u8>(std::ptr::null_mut(), 1, false);
assert_ne!(raw, 0);
assert!(epoch::tagged::pointer::<u8>(raw).is_null());
```

Пустоту проверяют через `pointer(raw).is_null()`. Сравнение слова с нулём
отвечает на другой вопрос: «слово вообще не трогали».

`with_mark` выставляет младший бит и сохраняет адрес и поколение. После
2^16 успешных CAS на тот же адрес `wrapping_add` повторяет поколение.
Это учебный предел: дальше ABA на этом адресе снова возможна.

### Барьеры памяти

Ниже — соответствия, на которых держится крейт. Имена — варианты `Ordering`.

| Место | Ordering | Зачем |
| --- | --- | --- |
| CAS звена структуры (`head`, `tail`, `next`) | успех `Release`, неудача `Acquire` | Успех публикует поля узла, записанные до CAS. Неудача перечитывает чужое слово. |
| Запись `next` ещё не опубликованного узла | `Relaxed` | Узел пока частный. Наружу его вывозит последующий `Release`-CAS. |
| Чтение уже опубликованного `next` | `Acquire` | Видит запись, которую опубликовал `Release`-CAS звена. |
| Занятие слота домена | успех `AcqRel` | Поток видит, что предыдущий владелец записал «неактивен» и отпустил слот. |
| Публикация hazard-адреса, локальной эпохи, тикета deferred | `Release`, затем `fence(SeqCst)` | Сканер либо видит публикацию, либо его unlink становится виден повторному чтению этого потока. |
| Повторное чтение слова в `protect` и в `pin` | `Acquire` | Стоит после барьера и после публикации слота. |
| Продвижение эпохи, переключение когорты RCU | `SeqCst` | Эти записи стоят в одном общем порядке с барьером читателя. |
| Тикет deferred `fetch_add` | `Release` | Unlink, сделанный раньше в этом потоке, виден тому, кто `Acquire`-прочитал новый тикет и только потом лезет в структуру. |
| Снятие защиты, «неактивен» | `Release` | Продвижение схемы не обгоняет чтения и `retire` этого потока. |

`SeqCst`-барьер в паре «читатель / сборщик» нужен потому, что одной пары
`Release`/`Acquire` на разных атомиках мало. Читатель пишет слот и потом
перечитывает структуру. Сборщик меняет структуру и потом читает слот.
Барьер не даёт перечитыванию структуры уехать выше публикации слота и не
даёт чтению слота уехать выше unlink.

Под фичей `loom` те же вызовы идут в модель `loom`. Смешивать `std`-атомики
и `loom`-атомики в одном сценарии нельзя: это разные типы. `Arc` домена и в
модели остаётся `std::sync::Arc`. `spin_loop` в обычной сборке —
`std::hint::spin_loop`, под фичей — `loom::thread::yield_now`: ждущий поток
RCU становится точкой переключения модели.

### Домен, слоты и limbo

У каждой коллекции свой домен. Домены не сливаются в процессный синглтон,
и тесты не делят отложенные узлы.

В домене `MAX_THREADS` слотов: 64 в обычной сборке и 4 под `loom`. Обход
64 слотов на каждом атомике раздувает модель, поэтому предел для неё ниже.
Поток занимает первый свободный слот CAS-ом `0 → 1` (успех — `AcqRel`) и
запоминает индекс в thread-local хранилище. Повторный вход в тот же домен с
того же потока слот заново не ищет. Регистрация — `Vec` в TLS, по одной
записи на указатель домена (`Arc::as_ptr`).

Слоты обёрнуты в `CachePadded`: выравнивание 128 байт. На линейке в 64 байта
это две линии, соседние потоки не пишут в одну и ту же.

`INACTIVE` — это `usize::MAX`. Для эпохи и deferred занятый слот с такой
меткой означает «поток в домене, но не в секции». В минимуме и в проверке
эпохи такое значение освобождение не держит.

Локальный limbo лежит в TLS рядом с индексом слота. Туда `retire` кладёт
`Retired`: сырой указатель и функцию уничтожения. Когда поток завершается,
деструктор TLS (в сборке без `loom`) гасит слот и перекладывает локальный
список в сиротский, под мьютексом домена. Иначе мёртвый поток навсегда
остаётся «активным» и останавливает эпоху, RCU или deferred. Hazard pointers
от эпохи не зависят, но занятый навсегда слот всё равно съедает лимит потоков.

Под `loom` деструктор TLS атомики модели не трогает. Регистрацию снимает
`free_all_for_shutdown`, пока модель ещё исполняет тест. Вызов идёт через
`LocalKey::try_with` и `RefCell::try_borrow_mut`, поэтому уже взятый borrow
не роняет путь остановки.

Вложенный вход в тот же домен на том же потоке увеличивает глубину.
Публикует и снимает защиту только внешний guard. У hazard pointers вложения
делят те же два слота: внутренний заход не заводит вторую пару указателей.
Guard не `Send`. `PhantomData<*const ()>` делает его `!Send` и `!Sync`. Его
нельзя перенести на другой поток: глубина и слот — состояние TLS.

`Drop` коллекции требует, чтобы других обращений уже не было. Живая цепочка
тогда уничтожается прямым обходом, без барьера. Уже отложенные узлы этого
потока и сироты забирает `free_all_for_shutdown`, тоже без ожидания барьера:
эксклюзивный доступ и есть гарантия, что читателей не осталось.

Практический порядок такой. Сначала `join` потоков, которые трогали
коллекцию, потом её `Drop`. Limbo потока, который ещё жив, лежит в его TLS.
`Drop` с другого потока этот список не видит. Если такой поток завершится
последним держателем `Arc` домена, узлы из сиротского списка утекут:
`Retired` сам память не освобождает. `abandon` у hazard pointers перекладывает
локальный список в мьютекс сирот и сам его не сканирует. Epoch и deferred на
выходе пытаются собрать сирот, а то, что барьер ещё держит, остаётся в
мьютексе до уничтожения домена. Для RCU limbo сразу лежит в очередях домена,
не в TLS, и остановка домена эти очереди видит.

На пути самих CAS стека, очереди и списка мьютекса нет. Мьютекс закрывает
сиротский список и очереди когорт RCU. Поток, дошедший до сборки limbo,
может на этом мьютексе подождать. Отравленный мьютекс читается через
`into_inner`: паника чужого потока не оставляет список недоступным.

Больше `MAX_THREADS` одновременных участников роняют домен паникой
`epoch: a domain admits at most N threads`.

### Hazard pointers

Схема Maged Michael. Поток защищает не «всю эпоху», а конкретные адреса,
которые сейчас читает.

У слота потока:

- флаг занятости;
- два опубликованных указателя.

Стеку хватает одного. Очереди нужны голова и следующий узел одновременно.
Таблице нужны текущий узел и предыдущий. Голова бакета — часть самой таблицы,
в слот она не пишется.

#### Протокол защиты

`Shield::protect(i, atomic)`:

1. `Acquire`-прочитать слово.
2. Записать в слот `i` голый адрес, без поколения и без метки, с `Release`.
   Сканер сравнивает адреса блоков. Два слова с одним адресом и разным
   поколением — одна и та же аллокация.
3. Поставить `SeqCst`-барьер.
4. `Acquire`-прочитать слово ещё раз.
5. Если слово совпало, его можно разыменовать. Иначе слот уже указывает на
   адрес, которого в структуре может не быть, и цикл начинается сначала.

`protect_ptr` записывает адрес, который этот поток уже проверил, и тоже
ставит барьер. Так таблица переносит узел из роли «текущий» в роль
«предыдущий», не открывая окно без защиты. Слот 0 записывается раньше, чем
слот 1 переезжает дальше.

`clear` зануляет один слот. Перед `retire` своего узла это обязательно:
иначе сборка увидит собственный слот и не уничтожит блок никогда.

`Drop` внешнего щита зануляет оба слота. Внутренний щит слоты не трогает.

#### Когда память уничтожается

`retire` кладёт узел в локальный список. Когда в нём не меньше
`2 * MAX_THREADS * 2` элементов (два раза по числу опубликованных слотов в
домене; в обычной сборке это 256, под `loom` — 16), поток делает скан:

1. `SeqCst`-барьер, парный барьеру в `protect`.
2. Снимок всех слотов через `Acquire`.
3. Всё, чего в снимке нет, — кандидаты на уничтожение. Остальное остаётся в limbo.
4. Перед самым `free` адрес проверяется ещё раз. `protect` мог опубликовать
   его уже после снимка. Ложное срабатывание на короткой неудачной попытке
   `protect` безопасно: узел поживёт в сиротском списке ещё один проход.

Порог — амортизация из статьи, а не условие корректности. Редкая сборка
просто дольше держит limbo. `try_reclaim` сканирует, не дожидаясь порога.

Долгий `protect` удерживает только тот адрес, который записан в слот.
Соседние узлы из-за этого потока не застревают. Это местная плата hazard
pointers: на каждый разыменованный адрес нужны запись в слот, барьер и
повторное чтение.

### Схема Epoch

Глобальный счётчик эпохи `e` и по слоту на поток: занят ли он и какую эпоху
опубликовал. Узел, отложенный в эпоху `e`, лежит в сумке `e % 3` и
уничтожается, когда глобальный номер дошёл до `e + 2`.

Трёх сумок хватает, потому что одновременно живы текущая эпоха и две
предыдущие. Когда номер становится `e + 2`, сумка эпохи `e` свободна, а
индекс `e % 3` можно занять снова.

#### Вход

`EpochDomain::pin`:

1. Занять слот, если поток ещё не в домене.
2. Для внешнего guard: прочитать глобальную эпоху, `Release`-записать её в
   слот, поставить `SeqCst`-барьер, перечитать глобальную. Если номер
   сменился, записать новый и повторить.
3. Вложенный `pin` только увеличивает глубину и эпоху заново не публикует.

Пока внешний guard жив, в слоте лежит номер, прочитанный на входе. `Drop`
внешнего guard пишет `INACTIVE` с `Release` и пробует продвинуть эпоху.

#### Почему освобождение ждёт две эпохи

Продвижение читает глобальный номер, ставит `SeqCst`-барьер и смотрит слоты.
Занятый слот с номером, отличным от текущего глобального и от `INACTIVE`,
шаг запрещает. Иначе `SeqCst`-CAS увеличивает номер на единицу (`wrapping_add`).

Пока guard опубликован в эпохе `e`, номер не уходит дальше `e + 1`. Шаг
`e → e + 1` такой guard пропускает: его номер совпадает с глобальным. Шаг
`e + 1 → e + 2` он останавливает. Значит, ссылки, взятые внутри pin эпохи
`e`, к моменту глобальной эпохи `e + 2` уже отпущены. Узлы, отложенные в
`e`, в этот момент уничтожаются. Проверка в коде:
`global.saturating_sub(epoch) >= 2`.

Числовой след одного потока:

| Шаг | Глобальная эпоха | Что с узлом, отложенным в 0 |
| --- | --- | --- |
| pin, retire в сумку `0 % 3` | 0 | лежит в сумке |
| unpin, первый успешный шаг | 1 | `1 - 0 < 2`, ещё жив |
| следующий шаг, других pin нет | 2 | `2 - 0 >= 2`, уничтожен |

`try_reclaim` пытается сделать два шага. Второго часто нет: поток, который
сам ещё держит pin в эпохе `e`, после первого шага оказывается строго ниже
глобального номера. Узел, отложенный только что, этот вызов переживает. Двух
шагов хватает, чтобы добить сумку, которая ждала как раз этого окна.

Если поток долго не собирал limbo и в слот `e % 3` приехала другая эпоха,
старые узлы перекладываются в overflow вместе со своим номером. Они не
смешиваются с новой сумкой и уничтожаются по тому же правилу `+ 2`.

Долгий pin удерживает освобождение для всех, кто отстал не больше чем на
одну эпоху. Это плата схемы: читать дёшево (одна публикация на операцию), а
заснувший внутри pin поток останавливает счётчик.

### Схема RCU

Userspace RCU на двух счётчиках читателей. Номера эпохи здесь нет.

Состояние домена:

- индекс текущей когорты, младший бит атомика `current`;
- два счётчика читателей;
- две очереди отложенных узлов, по одной на когорту;
- флаг «кто-то уже проводит grace period»;
- таблица слотов, чтобы соблюдать лимит потоков и гасить секцию при выходе.

#### Вход читателя

`RcuDomain::lock`:

1. Прочитать индекс когорты.
2. `fetch_add(1)` на её счётчик с `AcqRel`.
3. `SeqCst`-барьер.
4. Если индекс не сменился, секция началась.
5. Иначе откатить счётчик `fetch_sub` и повторить. Ожидание зовёт `spin_loop`.

Вложенный guard когорту заново не берёт: глубина растёт, счётчик увеличивал
только внешний вход. `Drop` внешнего guard сначала уменьшает счётчик и лишь
потом глубину. Обратный порядок открыл бы окно, в котором `retire` этого же
потока уже видит нулевую глубину, а счётчик всё ещё включает его самого.

#### Освобождение

`retire` отказывается работать внутри read-side секции этого потока и
паникует с текстом про ожидание собственного счётчика. Коллекции поэтому
закрывают guard блоком и вызывают `retire` снаружи. В стеке и очереди это
видно по отдельному блоку в `pop` и `dequeue`.

Дальше:

1. Узел кладётся в очередь текущей когорты.
2. Поток пытается захватить флаг синхронизации CAS-ом `0 → 1`. Если флаг уже
   занят, узел просто ждёт следующего прохода.
3. Захвативший читает индекс, `SeqCst`-записью переключает его на другую
   когорту и крутится, пока предыдущий счётчик не станет нулём.
4. Очередь предыдущей когорты забирается целиком и уничтожается.
5. Флаг сбрасывается через `Release`.

Читатель, который увеличил старый счётчик и ещё не перепроверил индекс,
либо откатится на шаге 5 входа, либо будет учтён в ожидании. Читатель,
успешно вошедший в новую когорту, сделал это после переключения, то есть
после `retire` и после unlink: вынутый узел ему уже не попадается.

Поток, который захватил проведение grace period, крутится, пока читатели
старой когорты не выйдут. Если флаг уже занят другим потоком, `retire`
возвращается сразу, а узел дожидается следующего прохода в очереди когорты.
Epoch, hazard pointers и deferred внутри `retire` чужой поток так не ждут.
`enqueue` узлы не откладывает. Ожидание на счётчике делает `dequeue`,
потому что ему есть кого класть в limbo.

Limbo RCU с момента `retire` лежит в очередях домена под мьютексом.
Остановка домена видит обе очереди. В debug-сборке `free_all_for_shutdown`
ещё проверяет, что оба счётчика читателей нулевые: эксклюзивный `Drop` и
есть причина, по которой они пусты.

### Схема Deferred

Отдельного номера эпохи и трёх сумок нет. Есть монотонный тикет.

`retire` делает `fetch_add(1)` с `Release` и запоминает старое значение как
штамп узла. Поток на входе в секцию публикует последний увиденный тикет:
`Acquire`-чтение счётчика, `Release`-запись в слот, `SeqCst`-барьер. Дальше
он читает структуру. На выходе пишет `INACTIVE` и пробует собрать limbo.

Уничтожается префикс со штампом строго меньше минимума по занятым слотам.
Свободные слоты в минимуме не участвуют. Неактивный занятый слот хранит
`usize::MAX` и минимум не опускает. Если секций нет вовсе, минимум остаётся
`usize::MAX`, и любой штамп, который ещё не переполнил `usize`, меньше него.

Строгое сравнение — часть протокола.

- Читатель загрузил тикет `5` и опубликовал `5`. Следующий `retire` получит
  штамп `5`. `5 < 5` ложно, узел остаётся. Этот читатель мог войти, ещё не
  увидев unlink.
- `retire` со штампом `5` случился раньше, тикет стал `6`, запись была
  `Release`. Читатель `Acquire`-прочитал `6`, опубликовал `6` и только потом
  полез в структуру: unlink он видит. Штамп `5 < 6`, узел можно уничтожать,
  даже пока секция ещё идёт.

`retire` изнутри секции разрешён. Своя опубликованная метка не старше штампа,
который секция сама только что взяла, поэтому свой узел не уничтожится до
`Drop` guard. Этим deferred отличается от RCU, где такой вызов паникует.

Долгая секция удерживает все узлы со штампом больше либо равным её тикету.
Заснувший внутри `enter` поток останавливает сборку так же, как долгий pin.

Переполнение тикета — теоретический предел на `usize::MAX` шагов. Раньше
него кончается 16-битное поколение указателя. Глобальная эпоха использует
`wrapping_add` и `saturating_sub`, поэтому у неё тот же теоретический предел
по завороту `usize`.

### Сравнение схем

| | Что публикует читатель | Когда вызывают `retire` | Когда память уничтожается | Цена чтения | Что держит заснувший поток |
| --- | --- | --- | --- | --- | --- |
| Hazard pointers | Конкретные адреса в двух слотах | Сразу, в локальный список. Свой слот перед этим снимают | Адреса нет ни в одном слоте | На каждый адрес: чтение, запись, барьер, повторное чтение | Только записанные им адреса |
| Epoch | Номер глобальной эпохи | Внутри pin, в сумку `e % 3` | Глобальная эпоха ≥ `e + 2` | Одна публикация на операцию | Продвижение дальше `e + 1` |
| RCU | Плюс один к счётчику когорты | Только снаружи read-side секции | Старый счётчик обнулился после переключения когорты | `fetch_add`, барьер, повторная проверка индекса | `retire` крутится, пока эта секция не кончится |
| Deferred | Увиденный тикет | Можно внутри секции | Штамп строго меньше минимума активных меток | Запись тикета и барьер | Все штампы ≥ его тикета |

Общее у всех четырёх: узел сначала становится недостижимым из структуры, и
только потом попадает в limbo. Общее ограничение: секция, которая не
кончается, откладывает уничтожение. Hazard pointers откладывают точечно,
остальные три — пакетом.

### Стек Трайбера

Голова — одно упакованное слово. Пустой стек — нулевой адрес и какое-то
поколение; проверяется адрес, а не равенство слова нулю.

Узел: канарейка, `Option<T>`, атомарный голый указатель `next`. Канарейка
проверяется обычным `assert` при уничтожении и затем затирается нулём.
Повторный `free` падает и в release. Это ловушка на инвариант, а не защита
от порчи. Канарейка стека — `0xE4C0_FFEE_E4C0_FFEE`.

`push`:

1. Выделить узел. Он виден только текущему потоку.
2. Прочитать голову через `Acquire`.
3. Записать её адрес в `next` через `Relaxed`.
4. CAS головы на новый узел с поколением на единицу больше. Успех — `Release`.

Чужой узел не разыменовывается, поэтому ни hazard-слот, ни pin, ни когорта,
ни тикет не нужны. Поток, который только кладёт в стек, слот домена вообще
не занимает.

`pop`:

1. Войти в схему.
2. Прочитать голову. Для hazard pointers это `protect` слота 0, для остальных
   схем чтение внутри уже открытой секции.
3. Нулевой адрес — стек пуст.
4. Прочитать `next`.
5. CAS головы на этот `next` с новым поколением.
6. Забрать `T` из узла.
7. Для hazard pointers снять слот.
8. `retire`. У RCU шаг 8 стоит после закрытия guard. У epoch и deferred guard
   в этот момент ещё жив и сам не даёт уничтожить узел.

Значение возвращается вызывающему. `Drop` для `T` случается у вызывающего, а
не в момент `free` узла: внутри узла к этому времени уже пустой `Option`.

Видимость операции другим потокам даёт успешный CAS головы.

`Drop` стека идёт по цепочке от головы и уничтожает узлы сразу, затем
добирает limbo домена.

Типы: `HazardStack<T>`, `EpochStack<T>`, `RcuStack<T>`, `DeferredStack<T>`.
Граница `Send` на `T` такая же, как у `Mutex`: владение значением переезжает
между потоками, делят они только `&Self`.

### Очередь Michael–Scott

Очередь начинается с фиктивного узла. В пустой очереди `head` и `tail`
указывают на него, его `next` пуст, значения в нём нет. Данные лежат в узлах
следом за головой. `next` — упакованное слово, потому что после публикации
его меняет CAS. Канарейка очереди — `0xC0FF_EE00_C0FF_EE00`.

```text
enqueue(1), enqueue(2):

head                                    tail
  |                                       |
  v                                       v
+------+     +------+     +------+
| dummy|---->|  1   |---->|  2   |----> null
+------+     +------+     +------+

dequeue забирает 1 и retire-ит dummy.
Новая голова — бывший узел с 1, уже без значения.

head                 tail
  |                    |
  v                    v
+------+     +------+
| dummy|---->|  2   |----> null
| (был 1)    +------+
+------+
```

В limbo уходит старая голова, а не узел, из которого только что забрали
значение. Этот узел становится новой фиктивной головой. Сосед ещё может
читать `next` старой головы, поэтому её и нельзя уничтожать сразу.

`enqueue`:

1. Выделить узел со значением и пустым `next`.
2. Войти в схему. Хвост предстоит разыменовать.
3. Прочитать `tail`. Hazard pointers защищают его слотом 0.
4. Прочитать `tail.next`.
5. Если хвост за это время сменился, начать сначала.
6. Если `next` пуст, CAS-ом прицепить новый узел. Успех публикует значение.
   Затем попытаться сдвинуть `tail` на новый узел. Проигрыш этого второго CAS
   безопасен: узел уже в списке, хвост подтянет следующий заход.
7. Если `next` не пуст, хвост отстал. CAS сдвигает его на один узел вперёд,
   и цикл ищет конец снова. Прыжка через узел нет: за итерацию хвост проходит
   одно звено.

`dequeue`:

1. Войти в схему.
2. Прочитать голову. Hazard pointers: слот 0.
3. Прочитать `head.next`. Hazard pointers: слот 1, потому что значение лежит
   в этом узле и его тоже надо удержать.
4. Если голова сменилась, начать сначала.
5. Если указатель головы совпал с указателем хвоста:
   - `next` пуст — очередь пуста;
   - иначе хвост отстал, его подтягивают, и цикл повторяется.
6. Если при расхождении головы и хвоста `next` всё ещё пуст, снимок устарел.
   Пустой указатель не разыменовывается, цикл читает голову заново.
7. CAS сдвигает голову на `next`.
8. Значение забирается из нового головного узла. Старая голова уходит в
   `retire`. У hazard pointers оба слота перед этим снимаются. У RCU guard уже закрыт.

Видимость `enqueue` даёт успешный CAS звена `next`, а не подтягивание хвоста.
Видимость `dequeue` даёт успешный CAS головы.

`Drop` обходит цепочку от головы, включая фиктивный узел, и затем останавливает
домен. `head` и `tail` зануляются, чтобы повторный обход никуда не пришёл.

Типы: `HazardQueue<T>`, `EpochQueue<T>`, `RcuQueue<T>`, `DeferredQueue<T>`.
Граница на `T` — `Send`, как у стека.

### Хеш-таблица

`HazardMap<K, V>` и `EpochMap<K, V>`. Число бакетов задаётся в `new` и больше
не меняется: ноль бакетов — паника. Смена массива бакетов — отдельная задача,
там пришлось бы защищать ещё и указатель на сам массив.

Бакет выбирается как `DefaultHasher(key) % buckets`. Случайного семени на
экземпляр нет. Ключ — `Hash + Ord`. Порядок нужен списку, хеш — только чтобы
выбрать бакет.

Каждый бакет — голова отсортированного списка Майкла. Голова лежит в теле
таблицы и в limbo не попадает. Узел хранит ключ, значение и упакованный
`next`. После публикации ключ и значение не меняются: повторный `insert`
старое значение не подменяет. Канарейка таблицы — `0xA11C_E5A1_1CE5_A110`.

Удаление в два шага.

1. Логическое: CAS ставит младший бит `next`. С этого момента поиск не имеет
   права вернуть ключ. Поколение и адрес в этом CAS те же.
2. Физическое: CAS предшественника переставляет звено на узел следом за
   помеченным. В новое слово пишется следующее поколение.

`remove` возвращает `true` после успешной пометки, даже если физический CAS
проиграл. Вырезку и `retire` тогда делает поиск, который наткнётся на метку.
В limbo узел кладёт только поток, чей CAS вырезки выиграл, поэтому повторного
`retire` нет. Помеченный, но ещё висящий в цепочке узел заберёт `Drop` таблицы:
обход идёт по живым звеньям и такие узлы ещё там.

Поиск идёт по возрастанию ключа и по дороге помогает вырезать помеченное.

- Ключ найден — `Found` с указателем на звено предшественника.
- Встретился больший ключ или конец списка — `Missing`: сюда вставляют.
- Предшественник больше не указывает на текущий узел — обход устарел, поиск с головы бакета.
- Для hazard pointers слот 1 занимает текущий узел до чтения его полей.
  Перед шагом вперёд `protect_ptr` записывает его же в слот 0. Порядок важен:
  нельзя сначала отпустить текущий адрес и только потом защитить его как предыдущий.

`insert`:

- ключ уже есть — новый узел нигде не опубликован, его уничтожают сразу,
  метод возвращает `false`, старое значение остаётся;
- ключа нет — `next` нового узла указывает на место вставки, CAS
  предшественника публикует узел, метод возвращает `true`;
- CAS проиграл — узел остаётся локальным, поиск места начинается заново.

`get` требует `V: Clone`. Метод копирует значение, пока схема держит узел, и
ссылку наружу не отдаёт. Если до или после клона на узле стоит метка, поиск
повторяется и эта копия не возвращается. К моменту возврата guard уже можно
уничтожить: у вызывающего своя копия.

`Send` и `Sync` для карты требуют `Send + Sync` и от ключа, и от значения.
Ключ сравнивают несколько потоков, значение клонируют под защитой. У стека и
очереди значение никому не показывают, его отдают одному потоку, поэтому там
хватает `Send`.

RCU- и deferred-таблиц в крейте нет. Их обход совпал бы с epoch: одна секция
на всю операцию, без поузловых слотов.

`Drop` ключа и значения случается в момент физического освобождения узла.
Это может быть внутри `retire`, то есть ещё в секции удаляющего потока. Из
такого `Drop` нельзя снова входить в ту же таблицу. У hazard pointers
вложенный заход пишет в те же два слота и затирает защиту внешнего обхода.

### Публичный API

Коллекции экспортируются из корня крейта.

| Тип | Конструктор | Операции | Границы |
| --- | --- | --- | --- |
| `HazardStack<T>`, `EpochStack<T>`, `RcuStack<T>`, `DeferredStack<T>` | `new`, также `Default` | `push`, `pop -> Option<T>` | `T: Send`, чтобы тип был `Send + Sync` |
| `HazardQueue<T>`, `EpochQueue<T>`, `RcuQueue<T>`, `DeferredQueue<T>` | `new`, также `Default` | `enqueue`, `dequeue -> Option<T>` | `T: Send` |
| `HazardMap<K, V>`, `EpochMap<K, V>` | `new(buckets)` | `insert -> bool`, `remove -> bool`, `get -> Option<V>` | `K: Hash + Ord + Send + Sync`, `V: Send + Sync`, для `get` ещё `V: Clone` |

`insert` возвращает `false`, если ключ уже был. `remove` возвращает `false`,
если ключа не было. Значение `remove` наружу не отдаёт: оно уничтожается
вместе с узлом.

Модуль `reclaim` отдаёт домены. Все `new` возвращают `Arc<Domain>`.

| Тип | Вход | Отложить узел | Принудительная попытка сборки |
| --- | --- | --- | --- |
| `HazardDomain`, `Shield` | `HazardDomain::enter` | `Shield::retire` | `try_reclaim` |
| `EpochDomain`, `EpochGuard` | `EpochDomain::pin` | `EpochGuard::retire` | `try_reclaim` |
| `RcuDomain`, `RcuGuard` | `RcuDomain::lock` | `RcuDomain::retire` | `try_reclaim` |
| `DeferredDomain`, `DeferredGuard` | `DeferredDomain::enter` | `retire` на домене и на guard | `try_reclaim` |

Дополнительно у щита hazard pointers: `protect`, `protect_ptr`, `clear`.
Индекс слота — 0 или 1, иначе паника.

`Retired::new` небезопасен. Указатель должен быть блоком, который понимает
переданная функция уничтожения. Функцию вызовут один раз, и после этого
указателем никто не пользуется.

`tagged` экспортирует `pack`, `pointer`, `tag`, `is_marked`, `with_mark`.

`reclaim::MAX_THREADS` — 64 или 4, если крейт собран с `--features loom`.

Guard и щит заимствуют домен и несут `PhantomData<*const ()>`, поэтому они
`!Send` и `!Sync`. Между потоками переносят коллекцию, а не guard.

### Контракт unsafe

Публичная коллекция держит эти правила внутри. Прямой вызов домена переносит
последние пункты на вызывающего.

- Указатель в `Retired` выделен как тот тип, который уничтожает `drop_fn`.
- `drop_fn` вызывается ровно один раз и сама забирает значение в собственность.
- К моменту `retire` узел уже недостижим из структуры. Потоки, которые ещё
  могут читать его поля, сидят в защите схемы: слот, pin, когорта или
  опубликованный тикет.
- `Release`-CAS, опубликовавший узел, стоит в программе раньше `retire`.
- Для RCU `retire` вызывается при нулевой глубине read-side секции этого потока.
- Для hazard pointers слот, в котором ещё записан адрес этого узла, снят до
  `retire`, если это слот того же потока.
- `Drop` коллекции не пересекается с чужими вызовами её методов.
- Канарейка в узле к моменту уничтожения всё ещё равна константе модуля.
  Иначе это повторный `free`, и `assert` останавливает поток.

`unsafe impl Send/Sync` стоит на коллекциях, не на доменах. Домен содержит
только адреса и счётчики. Коллекция дополнительно обещает, что `T` (и ключ
с значением у карты) можно безопасно передать другому потоку в тех границах,
которые указаны на `impl`.

### Паники и assert

Строки ниже — часть крейта. Они на английском, как и комментарии.

| Где | Текст | Когда |
| --- | --- | --- |
| `claim_slot` каждого домена | `epoch: a domain admits at most {MAX_THREADS} threads` | Все слоты заняты. |
| `Shield::protect`, `protect_ptr`, `clear` | `a thread has only two hazard slots` | Индекс не 0 и не 1. |
| `RcuDomain::retire` | `RCU: retire inside a read-side critical section would wait on this thread's own reader count` | `retire` при ненулевой глубине read-side секции этого потока. |
| `with_local` | `thread has not entered the … domain` | `retire` или другая операция с TLS без предшествующего `enter` / `pin` / `lock` на этом потоке. |
| `HazardMap::new`, `EpochMap::new` | `at least one bucket is required` | `buckets == 0`. |
| `drop_node` | `double free of a stack/queue/map node` | Канарейка не равна константе модуля. Остаётся и в release. |
| `tagged::pack` | сообщения про выравнивание и старшие 16 бит | Только `debug_assert`. |
| корень `tagged` | `epoch requires 64-bit pointers` | `assert` на `usize::BITS` во время компиляции. |

`debug_assert_eq!(depth, 0)` в `Drop` внешнего guard проверяет, что учёт
глубины совпал с флагом внешнего входа. Остановка RCU в debug проверяет, что
оба счётчика читателей нулевые.

### Проверки

```sh
cargo test
cargo test --features loom
cargo miri test
```

`cargo test` гоняет `std`-атомики.

`tests/reclaim.rs` бьёт по доменам напрямую. «Бомба» — блок, чей `Drop`
поднимает флаг. Пока флаг опущен, физического освобождения не было.

- Защищённый hazard-указатель не уничтожается, после `clear` уничтожается.
- Pin эпохи и секция deferred держат узел до выхода, в том числе когда `retire` делает другой поток.
- RCU не уничтожает узел, пока читатель держит когорту: другой поток крутит `retire`, флаг остаётся опущенным, после `drop(guard)` узел освобождается.
- Скан hazard pointers видит слот чужого потока.

`tests/collections.rs` для каждой реализации проверяет три вещи.

- Стек отдаёт LIFO, очередь — FIFO, таблица совпадает со смесью `insert` / `get` / `remove`, повторный `insert` не затирает значение.
- `Drop` значения случается один раз. Для стека и очереди считаются уже вынутые значения: узел к этому моменту пуст. Для таблицы неудачный `insert` роняет новое значение сразу, успешный `remove` может отложить `Drop` до физического освобождения, остаток добивает `Drop` карты.
- Несколько потоков не теряют и не дублируют элементы. Обычный прогон — 4 потока и 200 операций. Под Miri — 2 потока и 16 операций, интерпретатор дорогой.

`cargo test --features loom` подменяет атомики крейта на атомики модели.
Интеграционные тесты `reclaim.rs` и `collections.rs` в этой сборке выключены,
вместо них собирается `tests/loom.rs`. Модульные тесты упаковки указателя
остаются. Модель короткая: `preemption_bound = 2`, не больше 8000 веток, два
потока и две-три операции. Она проверяет, что стек не теряет пару элементов
(`HazardStack`, `EpochStack`, `RcuStack`, `DeferredStack`), что очередь одного
производителя `EpochQueue` сохраняет FIFO, и что epoch не уничтожает блок,
пока другой поток держит pin. Это перебор маленького сценария, а не покрытие
всех путей четырёх схем.

`cargo miri test` гоняет функциональный набор без фичи `loom`. Вместе с loom
Miri не запускается: у модели свои атомики.

Канарейки и `assert` на двойное освобождение остаются в release-сборке тестов.
`debug_assert` в `pack` проверяет выравнивание и старшие биты адреса только в debug.

### Карта репозитория

```text
src/lib.rs                 корень: реэкспорт коллекций
src/atomic.rs              std или loom
src/pad.rs                 слот на своей кеш-линейке
src/tagged.rs              адрес, поколение, метка
src/retired.rs             limbo-блок
src/reclaim/mod.rs         лимит потоков и INACTIVE
src/reclaim/hazard.rs      hazard pointers
src/reclaim/epoch.rs       три сумки
src/reclaim/rcu.rs         две когорты
src/reclaim/deferred.rs    тикеты
src/stack/                 стек Трайбера, файл на схему
src/queue/                 очередь Michael–Scott, файл на схему
src/map/                   таблица, hazard и epoch
examples/stack.rs          два потока, hazard-стек
examples/queue.rs          FIFO, hazard-очередь
tests/reclaim.rs           барьеры на «бомбах»
tests/collections.rs       порядок, Drop, гонки
tests/loom.rs              маленькие модели
```

Фича `loom` необязательная и включает зависимость `loom` 0.7. Других
зависимостей у крейта нет.

### Ограничения

- Не больше 64 потоков на домен и не больше 4 в сборке с `loom`. Лишний поток паникует при входе в домен.
- Хеш-таблица не растёт. Хеш — `DefaultHasher` без семени на экземпляр, бакет — остаток от деления.
- Поколение указателя — 16 бит и заворачивается. Тикет deferred и номер эпохи заворачиваются только вместе с `usize`.
- Долгий pin, read-side секция или секция deferred удерживает освобождение. Hazard pointers удерживают только опубликованные адреса.
- `retire` внутри read-side секции RCU на том же потоке паникует. В коллекциях этот вызов стоит после guard.
- Вложенный вход в hazard-домен делит два слота с внешним. `Drop` значения таблицы не должен изнутри снова трогать эту же таблицу.
- Сиротский список и очереди RCU взяты под `Mutex`. Сам CAS структуры его не берёт, сборка limbo — берёт.
- `get` отдаёт клон. Ссылку в reclaimable память API не выдаёт.
- Повторный `insert` не обновляет значение.
- `remove` не возвращает старое значение. `true` означает логическое удаление; физическую вырезку может закончить другой поиск.
- Живую коллекцию нельзя уничтожать, пока чужой поток ещё не завершил работу с ней и не отдал свой TLS. Иначе его limbo может пережить домен и утечь, а не освободиться.
- Только 64-битные указатели. Старшие 16 бит адреса должны быть нулевыми.
- Назначение крейта — чтение алгоритма рядом с атомиком. По роли в экосистеме Rust рядом стоит `crossbeam-epoch`: у него нет жёсткого потолка потоков и другой инженерный объём.

### Словарь

| Слово | Что это в крейте |
| --- | --- |
| ABA | CAS видит прежний адрес и считает, что звено не менялось, хотя узел уже вынимали и вставляли снова. Закрывается поколением в том же слове. |
| Бакет | Голова одного отсортированного списка. Живёт в теле таблицы. |
| Grace period | Отрезок, после которого читатели, видевшие старую структуру, точно вышли. У RCU это обнуление предыдущего счётчика. |
| Канарейка | Константа в начале узла. Повторный `free` обнуляет её и на следующем разе падает на `assert`. |
| Когорта | Одна из двух групп читателей RCU со своим счётчиком и своей очередью limbo. |
| Limbo | Узлы, уже вынутые из структуры и ещё не уничтоженные. |
| Метка | Младший бит звена. Стоит — ключ логически удалён. |
| Pin | Вход в эпоху: слот публикует глобальный номер до чтения структуры. |
| Поколение | Старшие 16 бит слова. Растёт на успешном CAS. |
| Retire | Положить вынутый узел в limbo. |
| Сирота | Limbo потока, который уже завершился. Лежит в домене, пока какой-нибудь скан или `Drop` коллекции его не заберёт. |
| Слот | Запись одного потока в домене: занятость и опубликованная защита. |
| Тикет | Монотонный номер deferred. `retire` берёт следующий, секция публикует последний увиденный. |
| Фиктивный узел | Голова очереди без значения. `dequeue` снимает его, а данные читает из следующего. |
| Щит (`Shield`) | Окно hazard pointers на текущем потоке. Пока оно живо, поток имеет право писать в свои два слота. |

### Ориентиры

Реализации учебные и короче статей, но имена алгоритмов совпадают с этими текстами.

- R. K. Treiber. *Systems Programming: Coping with Parallelism.* IBM Technical Report RJ 5118, 1986. Стек на CAS головы.
- M. M. Michael, M. L. Scott. *Simple, Fast, and Practical Non-Blocking and Blocking Concurrent Queue Algorithms.* PODC 1996. Очередь с фиктивной головой и подтягиванием хвоста.
- T. L. Harris. *A Pragmatic Implementation of Non-Blocking Linked-Lists.* DISC 2001. Младший бит как логическое удаление.
- M. M. Michael. *High Performance Dynamic Lock-Free Hash Tables and List-Based Sets.* SPAA 2002. Список с пометкой и вырезкой, в том числе помощь чужому удалению при поиске.
- M. M. Michael. *Hazard Pointers: Safe Memory Reclamation for Lock-Free Objects.* IEEE TPDS, 2004. Протокол «опубликовать адрес, барьер, перечитать» и порог `2 * число слотов`.
- K. Fraser. *Practical Lock-Freedom.* PhD thesis, University of Cambridge, 2004. Epoch-based reclamation. Три сумки `e % 3` и ожидание `e + 2` — та же схема, которую использует `crossbeam-epoch`.
- P. E. McKenney, пользовательный RCU в линии liburcu (Mathieu Desnoyers). Два счётчика и переключение индекса — этот вариант, а не quiescent-state RCU, где у читателя нет атомарного счётчика.
- Отложенные тикеты в `reclaim/deferred.rs` оставлены прямым вариантом: один счётчик и минимум по слотам. По роли это ближайший родственник эпохи. Промышленные схемы с интервалами (hazard eras и interval-based reclamation) устроены тоньше, и крейт их не повторяет.
