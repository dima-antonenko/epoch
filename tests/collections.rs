#![cfg(not(feature = "loom"))]

//! Functional checks of the collections, without loom.
//!
//! Each implementation is checked for order (LIFO, FIFO, or agreement with
//! a map), for exactly one `Drop` of the value, and for no lost elements
//! under several threads. Under Miri there are two threads and 16
//! operations: the interpreter is expensive. A normal run uses four threads
//! and 200 operations.
//!
//! For the stack and the queue the value's `Drop` is counted on the value
//! that was taken out, not on the node. In the map the value lives in the
//! node, so a failed `insert` drops the new value immediately, while a
//! successful `remove` may delay `Drop` until the node is physically freed.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use epoch::{
    DeferredQueue, DeferredStack, EpochMap, EpochQueue, EpochStack, HazardMap, HazardQueue,
    HazardStack, RcuQueue, RcuStack,
};

fn threads() -> usize {
    if cfg!(miri) { 2 } else { 4 }
}

fn ops() -> usize {
    if cfg!(miri) { 16 } else { 200 }
}

struct Probe {
    drops: Arc<AtomicUsize>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

trait StackApi<T>: Send + Sync + 'static {
    fn new() -> Self;
    fn push(&self, value: T);
    fn pop(&self) -> Option<T>;
}

trait QueueApi<T>: Send + Sync + 'static {
    fn new() -> Self;
    fn enqueue(&self, value: T);
    fn dequeue(&self) -> Option<T>;
}

macro_rules! stack_api {
    ($ty:ty) => {
        impl<T: Send + 'static> StackApi<T> for $ty {
            fn new() -> Self {
                <$ty>::new()
            }
            fn push(&self, value: T) {
                self.push(value);
            }
            fn pop(&self) -> Option<T> {
                self.pop()
            }
        }
    };
}

macro_rules! queue_api {
    ($ty:ty) => {
        impl<T: Send + 'static> QueueApi<T> for $ty {
            fn new() -> Self {
                <$ty>::new()
            }
            fn enqueue(&self, value: T) {
                self.enqueue(value);
            }
            fn dequeue(&self) -> Option<T> {
                self.dequeue()
            }
        }
    };
}

stack_api!(HazardStack<T>);
stack_api!(EpochStack<T>);
stack_api!(RcuStack<T>);
stack_api!(DeferredStack<T>);
queue_api!(HazardQueue<T>);
queue_api!(EpochQueue<T>);
queue_api!(RcuQueue<T>);
queue_api!(DeferredQueue<T>);

fn stack_lifo<S: StackApi<i32>>() {
    let stack = S::new();
    assert_eq!(stack.pop(), None);
    stack.push(1);
    stack.push(2);
    stack.push(3);
    assert_eq!(stack.pop(), Some(3));
    assert_eq!(stack.pop(), Some(2));
    assert_eq!(stack.pop(), Some(1));
    assert_eq!(stack.pop(), None);
}

fn stack_drops_each_value_once<S: StackApi<Probe>>() {
    let drops = Arc::new(AtomicUsize::new(0));
    let stack = S::new();
    for _ in 0..8 {
        stack.push(Probe {
            drops: Arc::clone(&drops),
        });
    }
    let mut taken = Vec::new();
    for _ in 0..5 {
        taken.push(stack.pop().unwrap());
    }
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(taken);
    assert_eq!(drops.load(Ordering::SeqCst), 5);
    drop(stack);
    assert_eq!(drops.load(Ordering::SeqCst), 8);
}

fn stack_concurrent<S: StackApi<usize>>() {
    let stack = Arc::new(S::new());
    let mut handles = Vec::new();
    for thread_id in 0..threads() {
        let stack = Arc::clone(&stack);
        handles.push(thread::spawn(move || {
            let mut got = Vec::new();
            for i in 0..ops() {
                stack.push(thread_id * 10_000 + i);
            }
            for _ in 0..ops() {
                if let Some(value) = stack.pop() {
                    got.push(value);
                }
            }
            got
        }));
    }
    let mut all = Vec::new();
    for handle in handles {
        all.extend(handle.join().unwrap());
    }
    while let Some(value) = stack.pop() {
        all.push(value);
    }
    all.sort_unstable();
    let mut expected = Vec::new();
    for thread_id in 0..threads() {
        for i in 0..ops() {
            expected.push(thread_id * 10_000 + i);
        }
    }
    assert_eq!(all, expected);
}

fn queue_fifo<Q: QueueApi<i32>>() {
    let queue = Q::new();
    assert_eq!(queue.dequeue(), None);
    queue.enqueue(1);
    queue.enqueue(2);
    queue.enqueue(3);
    assert_eq!(queue.dequeue(), Some(1));
    assert_eq!(queue.dequeue(), Some(2));
    assert_eq!(queue.dequeue(), Some(3));
    assert_eq!(queue.dequeue(), None);
}

fn queue_drops_each_value_once<Q: QueueApi<Probe>>() {
    let drops = Arc::new(AtomicUsize::new(0));
    let queue = Q::new();
    for _ in 0..8 {
        queue.enqueue(Probe {
            drops: Arc::clone(&drops),
        });
    }
    let mut taken = Vec::new();
    for _ in 0..5 {
        taken.push(queue.dequeue().unwrap());
    }
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(taken);
    assert_eq!(drops.load(Ordering::SeqCst), 5);
    drop(queue);
    assert_eq!(drops.load(Ordering::SeqCst), 8);
}

fn queue_concurrent<Q: QueueApi<usize>>() {
    let queue = Arc::new(Q::new());
    let mut handles = Vec::new();
    for thread_id in 0..threads() {
        let queue = Arc::clone(&queue);
        handles.push(thread::spawn(move || {
            let mut got = Vec::new();
            for i in 0..ops() {
                queue.enqueue(thread_id * 10_000 + i);
            }
            for _ in 0..ops() {
                if let Some(value) = queue.dequeue() {
                    got.push(value);
                }
            }
            got
        }));
    }
    let mut all = Vec::new();
    for handle in handles {
        all.extend(handle.join().unwrap());
    }
    while let Some(value) = queue.dequeue() {
        all.push(value);
    }
    all.sort_unstable();
    let mut expected = Vec::new();
    for thread_id in 0..threads() {
        for i in 0..ops() {
            expected.push(thread_id * 10_000 + i);
        }
    }
    assert_eq!(all, expected);
}

fn map_basic<M>(make: impl Fn(usize) -> M)
where
    M: MapApi<i32, i32>,
{
    let map = make(2);
    assert_eq!(map.get(&1), None);
    assert!(map.insert(2, 20));
    assert!(map.insert(1, 10));
    assert!(map.insert(3, 30));
    assert!(!map.insert(2, 21));
    assert_eq!(map.get(&1), Some(10));
    assert_eq!(map.get(&2), Some(20));
    assert_eq!(map.get(&3), Some(30));
    assert!(map.remove(&2));
    assert_eq!(map.get(&2), None);
    assert!(!map.remove(&2));
    assert_eq!(map.get(&1), Some(10));
    assert!(map.remove(&1));
    assert!(map.remove(&3));
    assert_eq!(map.get(&1), None);
    assert_eq!(map.get(&3), None);
}

fn map_drops<M>(make: impl Fn(usize) -> M)
where
    M: MapApi<i32, Probe>,
{
    let drops = Arc::new(AtomicUsize::new(0));
    let map = make(4);
    map.insert(
        1,
        Probe {
            drops: Arc::clone(&drops),
        },
    );
    assert!(!map.insert(
        1,
        Probe {
            drops: Arc::clone(&drops),
        },
    ));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    map.insert(
        2,
        Probe {
            drops: Arc::clone(&drops),
        },
    );
    assert!(map.remove(&1));
    drop(map);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
}

fn map_concurrent<M>(make: impl Fn(usize) -> M)
where
    M: MapApi<usize, usize> + Send + Sync + 'static,
{
    let map = Arc::new(make(8));
    let mut handles = Vec::new();
    for thread_id in 0..threads() {
        let map = Arc::clone(&map);
        handles.push(thread::spawn(move || {
            for i in 0..ops() {
                let key = thread_id * 10_000 + i;
                assert!(map.insert(key, key));
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    for thread_id in 0..threads() {
        for i in 0..ops() {
            let key = thread_id * 10_000 + i;
            assert_eq!(map.get(&key), Some(key));
        }
    }
    for thread_id in 0..threads() {
        for i in 0..ops() {
            let key = thread_id * 10_000 + i;
            if i % 2 == 0 {
                assert!(map.remove(&key));
            }
        }
    }
    for thread_id in 0..threads() {
        for i in 0..ops() {
            let key = thread_id * 10_000 + i;
            if i % 2 == 0 {
                assert_eq!(map.get(&key), None);
            } else {
                assert_eq!(map.get(&key), Some(key));
            }
        }
    }
}

trait MapApi<K, V> {
    fn insert(&self, key: K, value: V) -> bool;
    fn remove(&self, key: &K) -> bool;
    fn get(&self, key: &K) -> Option<V>
    where
        V: Clone;
}

impl<K, V> MapApi<K, V> for HazardMap<K, V>
where
    K: std::hash::Hash + Ord + Send + Sync,
    V: Send + Sync,
{
    fn insert(&self, key: K, value: V) -> bool {
        HazardMap::insert(self, key, value)
    }
    fn remove(&self, key: &K) -> bool {
        HazardMap::remove(self, key)
    }
    fn get(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        HazardMap::get(self, key)
    }
}

impl<K, V> MapApi<K, V> for EpochMap<K, V>
where
    K: std::hash::Hash + Ord + Send + Sync,
    V: Send + Sync,
{
    fn insert(&self, key: K, value: V) -> bool {
        EpochMap::insert(self, key, value)
    }
    fn remove(&self, key: &K) -> bool {
        EpochMap::remove(self, key)
    }
    fn get(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        EpochMap::get(self, key)
    }
}

fn run_stack<S32, SProbe, SUsize>()
where
    S32: StackApi<i32>,
    SProbe: StackApi<Probe>,
    SUsize: StackApi<usize>,
{
    stack_lifo::<S32>();
    stack_drops_each_value_once::<SProbe>();
    stack_concurrent::<SUsize>();
}

fn run_queue<Q32, QProbe, QUsize>()
where
    Q32: QueueApi<i32>,
    QProbe: QueueApi<Probe>,
    QUsize: QueueApi<usize>,
{
    queue_fifo::<Q32>();
    queue_drops_each_value_once::<QProbe>();
    queue_concurrent::<QUsize>();
}

#[test]
fn hazard_stack() {
    run_stack::<HazardStack<i32>, HazardStack<Probe>, HazardStack<usize>>();
}

#[test]
fn epoch_stack() {
    run_stack::<EpochStack<i32>, EpochStack<Probe>, EpochStack<usize>>();
}

#[test]
fn rcu_stack() {
    run_stack::<RcuStack<i32>, RcuStack<Probe>, RcuStack<usize>>();
}

#[test]
fn deferred_stack() {
    run_stack::<DeferredStack<i32>, DeferredStack<Probe>, DeferredStack<usize>>();
}

#[test]
fn hazard_queue() {
    run_queue::<HazardQueue<i32>, HazardQueue<Probe>, HazardQueue<usize>>();
}

#[test]
fn epoch_queue() {
    run_queue::<EpochQueue<i32>, EpochQueue<Probe>, EpochQueue<usize>>();
}

#[test]
fn rcu_queue() {
    run_queue::<RcuQueue<i32>, RcuQueue<Probe>, RcuQueue<usize>>();
}

#[test]
fn deferred_queue() {
    run_queue::<DeferredQueue<i32>, DeferredQueue<Probe>, DeferredQueue<usize>>();
}

#[test]
fn hazard_map_suite() {
    map_basic(|buckets| HazardMap::new(buckets));
    map_drops(|buckets| HazardMap::new(buckets));
    map_concurrent(|buckets| HazardMap::new(buckets));
}

#[test]
fn epoch_map_suite() {
    map_basic(|buckets| EpochMap::new(buckets));
    map_drops(|buckets| EpochMap::new(buckets));
    map_concurrent(|buckets| EpochMap::new(buckets));
}
