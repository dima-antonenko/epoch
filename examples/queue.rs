//! Michael–Scott queue on hazard pointers.
//!
//! The protection window is wider than the stack's. `enqueue` holds the
//! tail while it reads `next`. `dequeue` publishes two addresses at once:
//! the current dummy head and the next node, which holds the value. Both
//! slots are cleared before the old head is retired. FIFO order comes from
//! the linked list, not from the reclamation scheme.

use std::sync::Arc;
use std::thread;

use epoch::HazardQueue;

fn main() {
    let queue = Arc::new(HazardQueue::new());
    queue.enqueue(1);
    queue.enqueue(2);
    let worker = thread::spawn({
        let queue = Arc::clone(&queue);
        move || queue.dequeue()
    });
    let first = worker.join().unwrap();
    let second = queue.dequeue();
    assert_eq!((first, second), (Some(1), Some(2)));
    println!("queue: {first:?}, {second:?}");
}
