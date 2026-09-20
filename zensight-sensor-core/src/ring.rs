//! A bounded ring of recent records behind a read procedure (#1156).
//!
//! The logs sensor keeps its recent lines in one (`@rpc/logs/events`, #358)
//! and netflow its recent raw flows (`@rpc/netflow/flows`, RFC 11 §3): the
//! high-cardinality detail RFC 04 R3 forbids streaming is held here, pulled
//! on request, and evicted oldest-first past a capacity. Both crates carried
//! the same `Arc<Mutex<VecDeque<T>>>` with the same `push`; this is that,
//! once, with the capacity stored beside the deque so a caller cannot push
//! with the wrong one.
//!
//! The lock is a `std::sync::Mutex`: the critical sections are a push or a
//! snapshot, never an `.await`, and a poisoned lock is recovered rather than
//! propagated — a ring that panicked mid-push still holds whole records.

use std::collections::VecDeque;
use std::sync::Mutex;

/// See the module doc.
#[derive(Debug)]
pub struct BoundedRing<T> {
    capacity: usize,
    inner: Mutex<VecDeque<T>>,
}

impl<T> BoundedRing<T> {
    /// An empty ring holding at most `capacity` records (at least one).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        BoundedRing {
            capacity,
            inner: Mutex::new(VecDeque::with_capacity(capacity)),
        }
    }

    /// The bound this ring evicts past.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Append one record, evicting the oldest past the capacity.
    pub fn push(&self, record: T) {
        let mut r = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        r.push_back(record);
        while r.len() > self.capacity {
            r.pop_front();
        }
    }

    /// Read the ring under its lock — a snapshot or a filtered walk, never
    /// an `.await`. Oldest first, as pushed.
    pub fn with<R>(&self, f: impl FnOnce(&VecDeque<T>) -> R) -> R {
        let r = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(&r)
    }

    /// How many records are held.
    pub fn len(&self) -> usize {
        self.with(VecDeque::len)
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_oldest_past_capacity() {
        let ring = BoundedRing::new(3);
        for i in 0..5 {
            ring.push(i);
        }
        assert_eq!(ring.len(), 3);
        assert_eq!(
            ring.with(|r| r.iter().copied().collect::<Vec<_>>()),
            [2, 3, 4]
        );
    }

    #[test]
    fn a_zero_capacity_holds_one() {
        let ring = BoundedRing::new(0);
        ring.push("a");
        ring.push("b");
        assert_eq!(ring.capacity(), 1);
        assert_eq!(ring.with(|r| r.back().copied()), Some("b"));
    }
}
