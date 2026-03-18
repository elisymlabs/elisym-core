use std::collections::{HashSet, VecDeque};
use std::hash::Hash;

use nostr_sdk::RelayPoolNotification;

/// Max number of event IDs to keep for deduplication in subscription handlers.
pub(crate) const DEDUP_CAPACITY: usize = 10_000;

/// Receive the next notification, handling lagged broadcast receivers by
/// logging the skip and continuing. Returns `None` when the channel is closed.
pub(crate) async fn recv_notification(
    notifications: &mut tokio::sync::broadcast::Receiver<RelayPoolNotification>,
) -> Option<RelayPoolNotification> {
    loop {
        match notifications.recv().await {
            Ok(n) => return Some(n),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "Subscription receiver lagged, some events may have been missed");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
        }
    }
}

/// A bounded deduplication set that evicts the oldest entries when full.
///
/// Used in subscription handlers to prevent duplicate event delivery
/// without unbounded memory growth in long-running processes.
///
/// **Eviction caveat:** Once the capacity is reached, the oldest IDs are
/// evicted and could be re-delivered as "new". For long-running agents,
/// handle idempotency at the business logic layer (e.g., track processed
/// job IDs in persistent storage).
pub(crate) struct BoundedDedup<T> {
    set: HashSet<T>,
    order: VecDeque<T>,
    capacity: usize,
}

impl<T: Hash + Eq + Clone> BoundedDedup<T> {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            set: HashSet::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Returns `true` if the item is new (not seen before).
    /// Returns `false` if it's a duplicate.
    pub fn insert(&mut self, item: T) -> bool {
        if self.set.contains(&item) {
            return false;
        }
        if self.order.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.set.remove(&evicted);
            }
        }
        self.set.insert(item.clone());
        self.order.push_back(item);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dedup_basic() {
        let mut d = BoundedDedup::new(3);
        assert!(d.insert(1));
        assert!(d.insert(2));
        assert!(!d.insert(1)); // duplicate
        assert!(d.insert(3));
        assert_eq!(d.set.len(), 3);
    }

    #[test]
    fn test_dedup_eviction() {
        let mut d = BoundedDedup::new(3);
        assert!(d.insert(1));
        assert!(d.insert(2));
        assert!(d.insert(3));
        // Full — inserting 4 evicts 1 (oldest)
        assert!(d.insert(4));
        assert_eq!(d.set.len(), 3);
        assert!(d.insert(1)); // 1 was evicted, so it's "new" again
        // Now set is {3, 4, 1} — 2 was evicted when 1 was inserted
        assert!(d.insert(2)); // 2 was evicted too
        assert!(!d.insert(4)); // 4 is still in the set
    }

    #[test]
    fn test_dedup_zero_capacity_clamped_to_one() {
        let mut d = BoundedDedup::new(0);
        assert_eq!(d.capacity, 1);
        assert!(d.insert(1));
        assert!(!d.insert(1)); // duplicate
        // Capacity is 1, so inserting 2 evicts 1
        assert!(d.insert(2));
        assert!(d.insert(1)); // 1 was evicted, new again
    }

    #[test]
    fn test_dedup_all_unique_no_eviction() {
        let mut d = BoundedDedup::new(10);
        for i in 0..10 {
            assert!(d.insert(i), "item {} should be new", i);
        }
        assert_eq!(d.set.len(), 10);
        // All still present
        for i in 0..10 {
            assert!(!d.insert(i), "item {} should be duplicate", i);
        }
    }

    #[test]
    fn test_dedup_reinsert_after_eviction() {
        let mut d = BoundedDedup::new(3);
        assert!(d.insert(1));
        assert!(d.insert(2));
        assert!(d.insert(3));
        // Evict 1 by inserting 4
        assert!(d.insert(4));
        assert!(!d.set.contains(&1));
        // Re-insert 1 — should be "new"
        assert!(d.insert(1));
    }
}
