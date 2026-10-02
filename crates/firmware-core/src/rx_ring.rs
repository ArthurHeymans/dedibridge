//! Conservative progress accounting around a hardware-owned circular RX DMA.
//!
//! Position alone cannot distinguish zero progress from one or more full wraps.
//! Require observations sooner than half a ring could arrive at the configured
//! 8N1 baud, and keep unread lag below half capacity. Check before AND after a
//! synchronous copy, then commit; never publish an ambiguous copy. This is not
//! host-pause storage, nor proof of the platform timer/DMA/debug-freeze behavior.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmbiguousProgress;

pub struct Progress {
    capacity: usize,
    position: usize,
    unread: usize,
    last_us: u64,
    deadline_us: u64,
    failed: bool,
}
impl Progress {
    pub fn new(capacity: usize, baud: u32, now_us: u64) -> Self {
        assert!(capacity >= 4 && capacity.is_multiple_of(2) && baud != 0);
        Self {
            capacity,
            position: 0,
            unread: 0,
            last_us: now_us,
            deadline_us: (capacity / 2) as u64 * 10_000_000 / u64::from(baud),
            failed: false,
        }
    }
    /// Account producer progress without reading DMA-owned memory.
    pub fn observe(&mut self, position: usize, now_us: u64) -> Result<(), AmbiguousProgress> {
        if self.failed
            || position >= self.capacity
            || now_us
                .checked_sub(self.last_us)
                .is_none_or(|elapsed| elapsed >= self.deadline_us)
        {
            self.failed = true;
            return Err(AmbiguousProgress);
        }
        self.unread += (position + self.capacity - self.position) % self.capacity;
        self.position = position;
        self.last_us = now_us;
        if self.unread >= self.capacity / 2 {
            self.failed = true;
            return Err(AmbiguousProgress);
        }
        Ok(())
    }
    /// Last observed unread lag, not a bound on progress in an ambiguous epoch.
    pub fn pending(&self) -> usize {
        self.unread
    }
    /// Call only after a successful post-copy observation, without an await
    /// between copy/check/commit. A failed epoch must be physically restarted.
    pub fn commit(&mut self, copied: usize) -> Result<(), AmbiguousProgress> {
        if self.failed || copied > self.unread {
            self.failed = true;
            return Err(AmbiguousProgress);
        }
        self.unread -= copied;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wrap_and_short_tails_are_consumed_in_order() {
        let mut progress = Progress::new(4096, 3_000_000, 0);
        for turn in 1..=1000 {
            // 61-byte batches cross the physical end repeatedly, with a
            // smaller trailing copy left pending for the next observation.
            progress
                .observe((turn * 61) % 4096, turn as u64 * 204)
                .unwrap();
            progress.commit(60).unwrap();
            progress
                .observe((turn * 61) % 4096, turn as u64 * 204 + 1)
                .unwrap();
            progress.commit(1).unwrap();
        }
    }
    #[test]
    fn full_or_multiple_wraps_cannot_masquerade_as_empty() {
        for bytes in [4095, 4096, 4097, 8192] {
            let mut progress = Progress::new(4096, 3_000_000, 0);
            // Includes delayed/collapsed TC interrupts: elapsed time catches
            // ambiguity even when the visible producer position is identical.
            assert_eq!(
                progress.observe(bytes % 4096, bytes as u64 * 10_000_000 / 3_000_000),
                Err(AmbiguousProgress)
            );
            assert_eq!(progress.commit(0), Err(AmbiguousProgress));
        }
    }
    #[test]
    fn post_copy_lag_or_stall_discards_the_copy_and_flush_starts_a_new_epoch() {
        let mut progress = Progress::new(4096, 3_000_000, 0);
        progress.observe(2000, 6666).unwrap();
        assert_eq!(progress.observe(2048, 6826), Err(AmbiguousProgress));
        assert_eq!(progress.commit(61), Err(AmbiguousProgress));
        let mut progress = Progress::new(4096, 3_000_000, 10_000);
        progress.observe(61, 10_204).unwrap();
        assert_eq!(progress.observe(61, 20_000), Err(AmbiguousProgress));
        assert_eq!(progress.commit(61), Err(AmbiguousProgress));
        let mut restarted = Progress::new(4096, 3_000_000, 30_000);
        restarted.observe(1, 30_004).unwrap();
        restarted.commit(1).unwrap();
        assert_eq!(restarted.unread, 0);
    }
    #[test]
    fn cancelled_wait_does_not_consume_and_clock_reversal_fails_closed() {
        let mut progress = Progress::new(4096, 3_000_000, 1000);
        progress.observe(10, 1034).unwrap();
        // Waiting is cancellable; no partial commit or clearing live DMA.
        progress.observe(20, 1068).unwrap();
        progress.commit(20).unwrap();
        assert_eq!(progress.unread, 0);
        assert_eq!(progress.observe(20, 1000), Err(AmbiguousProgress));
    }
}
