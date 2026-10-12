use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

/// How often a waiting worker checks whether the run was cancelled.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// Caps the pixels that all workers decode and encode at the same time.
///
/// Memory grows with the pixel count (about 6 bytes per pixel between the
/// decoded image and the encoder), so a folder of very large photos would
/// otherwise need several gigabytes per worker. An image that alone exceeds
/// the budget still runs, but only when no other image holds a share.
pub struct PixelBudget {
    capacity: u64,
    in_use: Mutex<u64>,
    released: Condvar,
}

impl PixelBudget {
    pub const fn new(capacity: u64) -> Self {
        Self {
            capacity,
            in_use: Mutex::new(0),
            released: Condvar::new(),
        }
    }

    /// Waits until `pixels` fit, or returns `None` when the run is cancelled.
    pub fn acquire(&self, pixels: u64, cancelled: &AtomicBool) -> Option<PixelLease<'_>> {
        let mut in_use = self.in_use.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return None;
            }
            if *in_use == 0 || in_use.saturating_add(pixels) <= self.capacity {
                *in_use = in_use.saturating_add(pixels);
                return Some(PixelLease {
                    budget: self,
                    pixels,
                });
            }
            in_use = self
                .released
                .wait_timeout(in_use, CANCEL_POLL)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// Returns its pixels to the budget when dropped.
pub struct PixelLease<'a> {
    budget: &'a PixelBudget,
    pixels: u64,
}

impl Drop for PixelLease<'_> {
    fn drop(&mut self) {
        let mut in_use = self
            .budget
            .in_use
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *in_use = in_use.saturating_sub(self.pixels);
        self.budget.released.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::PixelBudget;

    #[test]
    fn images_share_the_budget() {
        let budget = PixelBudget::new(100);
        let cancelled = AtomicBool::new(false);
        let first = budget.acquire(60, &cancelled);
        let second = budget.acquire(40, &cancelled);
        assert!(first.is_some() && second.is_some());
    }

    #[test]
    fn an_oversized_image_runs_alone() {
        let budget = PixelBudget::new(100);
        let cancelled = AtomicBool::new(false);
        assert!(budget.acquire(500, &cancelled).is_some());
    }

    #[test]
    fn a_waiting_image_starts_when_pixels_are_released() {
        let budget = PixelBudget::new(100);
        let cancelled = AtomicBool::new(false);
        let (started, receiver) = mpsc::channel();
        thread::scope(|scope| {
            let held = budget.acquire(80, &cancelled);
            scope.spawn(|| {
                let lease = budget.acquire(50, &cancelled);
                started.send(lease.is_some()).ok();
            });
            assert!(receiver.recv_timeout(Duration::from_millis(300)).is_err());
            drop(held);
            assert_eq!(receiver.recv_timeout(Duration::from_secs(5)), Ok(true));
        });
    }

    #[test]
    fn cancellation_stops_waiting() {
        let budget = PixelBudget::new(100);
        let cancelled = AtomicBool::new(false);
        thread::scope(|scope| {
            let _held = budget.acquire(100, &cancelled);
            let waiter = scope.spawn(|| budget.acquire(10, &cancelled).is_none());
            cancelled.store(true, Ordering::Relaxed);
            assert!(waiter.join().unwrap_or(false));
        });
    }
}
