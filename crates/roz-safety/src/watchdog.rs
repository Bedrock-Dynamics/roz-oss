use parking_lot::Mutex;
use std::time::{Duration, Instant};

/// A software watchdog that must be petted periodically.
///
/// If the watchdog is not petted within the deadline, it expires.
pub struct WatchdogTimer {
    deadline: Duration,
    last_pet: Mutex<Instant>,
}

impl WatchdogTimer {
    /// Create a new watchdog timer with the given deadline.
    pub fn new(deadline: Duration) -> Self {
        Self {
            deadline,
            last_pet: Mutex::new(Instant::now()),
        }
    }

    /// Pet the watchdog, resetting the timer.
    pub fn pet(&self) {
        *self.last_pet.lock() = Instant::now();
    }

    /// Check if the watchdog has expired (deadline exceeded since last pet).
    pub fn is_expired(&self) -> bool {
        self.last_pet.lock().elapsed() >= self.deadline
    }

    /// Returns the time remaining before the watchdog expires.
    /// Returns `Duration::ZERO` if already expired.
    pub fn time_remaining(&self) -> Duration {
        let elapsed = self.last_pet.lock().elapsed();
        self.deadline.saturating_sub(elapsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn fresh_watchdog_is_not_expired() {
        let wd = WatchdogTimer::new(Duration::from_secs(10));
        assert!(!wd.is_expired(), "fresh watchdog should not be expired");
    }

    #[test]
    fn watchdog_expires_after_deadline() {
        let wd = WatchdogTimer::new(Duration::from_millis(50));
        thread::sleep(Duration::from_millis(100));
        assert!(wd.is_expired(), "watchdog should expire after deadline");
    }

    #[test]
    fn pet_resets_timer() {
        let wd = WatchdogTimer::new(Duration::from_millis(100));
        thread::sleep(Duration::from_millis(60));
        wd.pet();
        thread::sleep(Duration::from_millis(60));
        // Total sleep is 120ms, but we petted at 60ms, so only 60ms since last pet
        assert!(!wd.is_expired(), "watchdog should not be expired after pet");
    }

    #[test]
    fn time_remaining_decreases() {
        let wd = WatchdogTimer::new(Duration::from_millis(200));
        let initial = wd.time_remaining();
        thread::sleep(Duration::from_millis(50));
        let later = wd.time_remaining();
        assert!(
            later < initial,
            "time remaining should decrease over time: initial={initial:?}, later={later:?}",
        );
    }

    #[test]
    fn time_remaining_is_zero_when_expired() {
        let wd = WatchdogTimer::new(Duration::from_millis(50));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            wd.time_remaining(),
            Duration::ZERO,
            "time remaining should be zero when expired"
        );
    }
}
