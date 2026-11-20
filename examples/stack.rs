//! Treiber stack on hazard pointers.
//!
//! The protection window is the body of `pop`, from `enter` until `retire`.
//! The top address stays in a hazard slot while the thread reads `next` and
//! performs the CAS. `push` publishes a node nobody else can see yet and
//! does not claim a slot. The generation in the high bits of `head` closes
//! ABA separately from that protection.

use std::sync::Arc;
use std::thread;

use epoch::HazardStack;

fn main() {
    let stack = Arc::new(HazardStack::new());
    let worker = thread::spawn({
        let stack = Arc::clone(&stack);
        move || {
            stack.push(1);
            stack.pop()
        }
    });
    stack.push(2);
    let mut values = Vec::new();
    if let Some(value) = stack.pop() {
        values.push(value);
    }
    if let Some(value) = worker.join().unwrap() {
        values.push(value);
    }
    while let Some(value) = stack.pop() {
        values.push(value);
    }
    values.sort_unstable();
    assert_eq!(values, vec![1, 2]);
    println!("stack: {values:?}");
}
