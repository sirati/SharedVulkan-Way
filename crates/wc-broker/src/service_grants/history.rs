//! Bounded live-context replay history. These IDs are bookkeeping, never
//! native completion or durable release authority. Unknown gaps stay unknown.
use super::*;
/// Exact closed-grant intervals with checked coalescing and fixed capacity.
/// The protected owner retains its own private instance; creating another
/// history grants no access to that owner, reservations or physical free list.
pub struct ClosedGrantHistory {
    intervals: BTreeMap<u64, u64>,
    limit: usize,
}
impl ClosedGrantHistory {
    pub fn new(limit: usize) -> io::Result<Self> {
        if limit == 0 {
            return Err(denied("zero closed grant history capacity"));
        }
        Ok(Self {
            intervals: BTreeMap::new(),
            limit,
        })
    }
    /// Reserve one possible isolated terminal interval per existing grant plus
    /// the next candidate, BEFORE any admission/offer/exposure can occur.
    pub fn reserve_admission(&self, active: usize) -> io::Result<()> {
        if !self
            .intervals
            .len()
            .checked_add(active)
            .and_then(|n| n.checked_add(1))
            .is_some_and(|n| n <= self.limit)
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "closed grant history requires complete drain and fresh context",
            ));
        }
        Ok(())
    }
    pub fn is_closed(&self, grant: u64) -> bool {
        self.intervals
            .range(..=grant)
            .next_back()
            .is_some_and(|(_, end)| grant <= *end)
    }
    pub fn interval_count(&self) -> usize {
        self.intervals.len()
    }
    /// Record only after the actual current durable release succeeds. This
    /// method itself supplies no release/native authority and frees no bytes.
    pub fn record_closed(&mut self, grant: u64) -> io::Result<()> {
        if grant == 0 || self.is_closed(grant) {
            return Err(denied("zero or already closed grant identity"));
        }
        let left = self
            .intervals
            .range(..grant)
            .next_back()
            .map(|(start, end)| (*start, *end));
        let right = self
            .intervals
            .range(grant..)
            .next()
            .map(|(start, end)| (*start, *end));
        let merge_left = left.is_some_and(|(_, end)| end.checked_add(1) == Some(grant));
        let merge_right = right.is_some_and(|(start, _)| grant.checked_add(1) == Some(start));
        if !merge_left && !merge_right && self.intervals.len() >= self.limit {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "closed grant interval capacity was not reserved",
            ));
        }
        let start = if merge_left { left.unwrap().0 } else { grant };
        let end = if merge_right { right.unwrap().1 } else { grant };
        if merge_left {
            self.intervals.remove(&left.unwrap().0);
        }
        if merge_right {
            self.intervals.remove(&right.unwrap().0);
        }
        self.intervals.insert(start, end);
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sequential_reuse_regains_admission_without_growing_history() {
        let mut history = ClosedGrantHistory::new(3).unwrap();
        for grant in 1..1000 {
            history.reserve_admission(0).unwrap();
            history.record_closed(grant).unwrap();
            assert_eq!(history.interval_count(), 1);
        }
        assert!(history.is_closed(1));
        assert!(history.is_closed(999));
        assert!(!history.is_closed(1000));
    }
    #[test]
    fn gaps_and_reserved_terminal_capacity_survive_fragmentation_and_reverse_close() {
        let mut history = ClosedGrantHistory::new(4).unwrap();
        history.record_closed(2).unwrap();
        history.record_closed(6).unwrap();
        history.reserve_admission(1).unwrap(); // Both admitted grants can close.
        assert!(history.reserve_admission(2).is_err());
        history.record_closed(4).unwrap();
        history.record_closed(8).unwrap();
        assert!(!history.is_closed(1));
        assert!(!history.is_closed(3));
        assert!(!history.is_closed(5));
        assert!(!history.is_closed(7));
        assert!(history.record_closed(10).is_err());
        history.record_closed(7).unwrap();
        history.record_closed(5).unwrap();
        history.record_closed(3).unwrap();
        assert_eq!(history.interval_count(), 1);
        assert!(!history.is_closed(1));
        assert!(history.is_closed(2));
        assert!(history.is_closed(8));
    }
    #[test]
    fn maximum_id_coalescing_does_not_wrap_or_forget_lower_gaps() {
        let mut history = ClosedGrantHistory::new(3).unwrap();
        history.record_closed(u64::MAX).unwrap();
        history.record_closed(u64::MAX - 1).unwrap();
        assert_eq!(history.interval_count(), 1);
        assert!(!history.is_closed(1));
        assert!(!history.is_closed(u64::MAX - 2));
        assert!(history.record_closed(0).is_err());
        assert!(history.record_closed(u64::MAX).is_err());
    }
}
