//! Global priority request queue.
//!
//! Exactly one agent loop runs at a time (the fleet behind litellm is the
//! scarce resource). Queued requests are dequeued by priority — lower value
//! first (0 = ethan, 1 = valarie, -1 = default for everyone else). Named
//! tiers (priority >= 0) are FIFO so a user's consecutive messages keep
//! their send order; the default tier shuffles so nobody is systematically
//! last. Non-preemptive: a running request always finishes first.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Mutex;
use tokio::sync::oneshot;

struct Waiter {
    priority: i32,
    /// FIFO sequence for named tiers, random tiebreak for the default tier.
    tiebreak: u64,
    ready: oneshot::Sender<()>,
}

impl PartialEq for Waiter {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.tiebreak == other.tiebreak
    }
}
impl Eq for Waiter {}

impl PartialOrd for Waiter {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// BinaryHeap pops the "greatest" element — invert the comparison so the
// lowest priority value (then lowest tiebreak) pops first.
impl Ord for Waiter {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .priority
            .cmp(&self.priority)
            .then_with(|| other.tiebreak.cmp(&self.tiebreak))
    }
}

pub struct RequestQueue {
    inner: Mutex<Inner>,
    seq: AtomicU64,
}

struct Inner {
    waiters: BinaryHeap<Waiter>,
    running: bool,
}

impl Default for RequestQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestQueue {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                waiters: BinaryHeap::new(),
                running: false,
            }),
            seq: AtomicU64::new(0),
        }
    }

    /// Take a place in line. Returns a permit to hold for the duration of
    /// the request, and whether any waiting was required.
    pub async fn acquire(&self, priority: i32) -> (QueuePermit<'_>, bool) {
        let rx = {
            let mut inner = self.inner.lock().unwrap();
            if !inner.running && inner.waiters.is_empty() {
                inner.running = true;
                return (QueuePermit { queue: self }, false);
            }
            let (tx, rx) = oneshot::channel();
            let seq = self.seq.fetch_add(1, AtomicOrdering::Relaxed);
            let tiebreak = if priority >= 0 {
                seq
            } else {
                rand::random()
            };
            inner.waiters.push(Waiter {
                priority,
                tiebreak,
                ready: tx,
            });
            rx
        };
        // A dropped sender only means the queue itself is gone — proceed
        // rather than block forever.
        let _ = rx.await;
        (QueuePermit { queue: self }, true)
    }
}

/// While held, no other request starts. Dropping hands off to the
/// highest-priority waiter.
pub struct QueuePermit<'a> {
    queue: &'a RequestQueue,
}

impl Drop for QueuePermit<'_> {
    fn drop(&mut self) {
        let mut inner = self.queue.inner.lock().unwrap();
        while let Some(w) = inner.waiters.pop() {
            if w.ready.send(()).is_ok() {
                // `running` stays true — ownership transferred to the waiter.
                return;
            }
            // Waiter went away without collecting — try the next one.
        }
        inner.running = false;
    }
}
