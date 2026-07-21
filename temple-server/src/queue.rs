//! Priority request queues, one lane per model.
//!
//! Requests to the same model/backend are serialized (the GPU host behind
//! it is the scarce resource), but requests to DIFFERENT models run in
//! parallel — a gemma chat on anton never blocks a deepseek agentic loop
//! on son-of-anton. Within a lane, requests dequeue by priority — lower
//! value first (0 = ethan, 1 = valarie, -1 = default for everyone else).
//! Named tiers (priority >= 0) are FIFO so a user's consecutive messages
//! keep their send order; the default tier shuffles so nobody is
//! systematically last. Non-preemptive: a running request always finishes
//! first.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
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

#[derive(Default)]
struct Lane {
    waiters: BinaryHeap<Waiter>,
    running: bool,
}

pub struct RequestQueue {
    lanes: Mutex<HashMap<String, Lane>>,
    seq: AtomicU64,
}

impl Default for RequestQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestQueue {
    pub fn new() -> Self {
        Self {
            lanes: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
        }
    }

    /// Snapshot of a lane for user feedback: (a request is running, how
    /// many are waiting). Approximate by nature — it's a status hint, not
    /// a contract.
    pub fn lane_status(&self, lane: &str) -> (bool, usize) {
        let lanes = self.lanes.lock().unwrap();
        match lanes.get(lane) {
            Some(l) => (l.running, l.waiters.len()),
            None => (false, 0),
        }
    }

    /// Take a place in line for `lane` (typically the target model).
    /// Returns a permit to hold for the duration of the request, and
    /// whether any waiting was required.
    pub async fn acquire(&self, lane: &str, priority: i32) -> (QueuePermit<'_>, bool) {
        let rx = {
            let mut lanes = self.lanes.lock().unwrap();
            let lane_state = lanes.entry(lane.to_string()).or_default();
            if !lane_state.running && lane_state.waiters.is_empty() {
                lane_state.running = true;
                return (
                    QueuePermit {
                        queue: self,
                        lane: lane.to_string(),
                    },
                    false,
                );
            }
            let (tx, rx) = oneshot::channel();
            let seq = self.seq.fetch_add(1, AtomicOrdering::Relaxed);
            let tiebreak = if priority >= 0 {
                seq
            } else {
                rand::random()
            };
            lane_state.waiters.push(Waiter {
                priority,
                tiebreak,
                ready: tx,
            });
            rx
        };
        // A dropped sender only means the queue itself is gone — proceed
        // rather than block forever.
        let _ = rx.await;
        (
            QueuePermit {
                queue: self,
                lane: lane.to_string(),
            },
            true,
        )
    }
}

/// While held, no other request starts in this lane. Dropping hands off
/// to the highest-priority waiter in the lane.
pub struct QueuePermit<'a> {
    queue: &'a RequestQueue,
    lane: String,
}

impl Drop for QueuePermit<'_> {
    fn drop(&mut self) {
        let mut lanes = self.queue.lanes.lock().unwrap();
        let lane_state = match lanes.get_mut(&self.lane) {
            Some(l) => l,
            None => return,
        };
        while let Some(w) = lane_state.waiters.pop() {
            if w.ready.send(()).is_ok() {
                // `running` stays true — ownership transferred to the waiter.
                return;
            }
            // Waiter went away without collecting — try the next one.
        }
        lane_state.running = false;
    }
}
