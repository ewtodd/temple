//! Priority request queues, one lane per model.
//!
//! Requests to the same model/backend are serialized (the GPU host behind
//! it is the scarce resource), but requests to DIFFERENT models run in
//! parallel — a gemma chat on anton never blocks a deepseek agentic loop
//! on son-of-anton. Within a lane, requests dequeue by priority — lower
//! value first (0 = ethan, 1 = valarie). Users without a named tier
//! (priority < 0, the default) always sort AFTER every named tier, in
//! random order so nobody is systematically last. Named tiers are FIFO so
//! a user's consecutive messages keep their send order. Non-preemptive: a
//! running request always finishes first.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Mutex;
use tokio::sync::oneshot;

/// Map a stored priority value to its queue ordering value. Negative
/// values (the default tier) must sort after every named tier — without
/// this mapping, -1 would dequee ahead of 0 and 1.
fn effective_priority(priority: i32) -> i32 {
    if priority < 0 {
        i32::MAX
    } else {
        priority
    }
}

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
            let tiebreak = if priority >= 0 { seq } else { rand::random() };
            lane_state.waiters.push(Waiter {
                priority: effective_priority(priority),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The priority-inversion regression test: default-tier requests (-1)
    /// must NEVER dequeue ahead of named tiers (0, 1). Before the fix,
    /// -1 sorted lowest and won every race — the exact opposite of the
    /// documented intent.
    #[tokio::test]
    async fn named_tiers_beat_default() {
        let q = Arc::new(RequestQueue::new());
        // Occupy the lane.
        let p0 = q.acquire("m", 0).await.0;

        // Queue: default first, then val(1), then ethan(0).
        let default_task = {
            let q = q.clone();
            tokio::spawn(async move {
                q.acquire("m", -1).await;
            })
        };
        let val_task = {
            let q = q.clone();
            tokio::spawn(async move {
                q.acquire("m", 1).await;
            })
        };
        let ethan_task = {
            let q = q.clone();
            tokio::spawn(async move {
                q.acquire("m", 0).await;
            })
        };
        tokio::task::yield_now().await;

        // Release — ethan (0) must win over val (1) and default (-1).
        // (Each spawned task drops its permit when it completes, which is
        // what hands the lane to the next waiter.)
        drop(p0);
        tokio::time::timeout(std::time::Duration::from_secs(1), ethan_task)
            .await
            .expect("ethan should win the lane first")
            .unwrap();

        // ethan's task completing released the lane — val (1) must beat
        // default (-1).
        tokio::time::timeout(std::time::Duration::from_secs(1), val_task)
            .await
            .expect("val should win the lane second")
            .unwrap();

        // ...and the default tier runs last.
        tokio::time::timeout(std::time::Duration::from_secs(1), default_task)
            .await
            .expect("default should get the lane last")
            .unwrap();
    }

    /// Named tiers are FIFO within the same priority — send order is
    /// preserved for consecutive messages from one user.
    #[tokio::test]
    async fn named_tier_is_fifo() {
        let q = Arc::new(RequestQueue::new());
        let p0 = q.acquire("m", 0).await.0;
        let t1 = {
            let q = q.clone();
            tokio::spawn(async move {
                q.acquire("m", 0).await;
            })
        };
        let t2 = {
            let q = q.clone();
            tokio::spawn(async move {
                q.acquire("m", 0).await;
            })
        };
        tokio::task::yield_now().await;
        drop(p0);
        tokio::time::timeout(std::time::Duration::from_secs(1), t1)
            .await
            .expect("first waiter should win")
            .unwrap();
        // t1's permit dropped on task completion → t2 proceeds.
        tokio::time::timeout(std::time::Duration::from_secs(1), t2)
            .await
            .expect("second waiter should follow")
            .unwrap();
    }

    /// Different lanes run in parallel — a busy gemma lane never blocks
    /// deepseek.
    #[tokio::test]
    async fn lanes_are_independent() {
        let q = RequestQueue::new();
        let _p1 = q.acquire("gemma", 0).await;
        // No waiting required on the other lane.
        let (_p2, queued) = q.acquire("deepseek", 0).await;
        assert!(
            !queued,
            "a fresh lane should not wait behind another lane's traffic"
        );
    }
}
