use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// Watchdog that fires if no command arrives within the deadline.
pub struct CommandWatchdog {
    last_pet_ms: Arc<AtomicU64>,
    deadline: Duration,
}

impl CommandWatchdog {
    pub fn new(deadline: Duration) -> Self {
        let now = Self::now_ms();
        Self {
            last_pet_ms: Arc::new(AtomicU64::new(now)),
            deadline,
        }
    }

    /// Reset the watchdog timer. Call this on each received command.
    pub fn pet(&self) {
        self.last_pet_ms.store(Self::now_ms(), Ordering::Relaxed);
    }

    /// Check if the watchdog has expired.
    pub fn is_expired(&self) -> bool {
        let last = self.last_pet_ms.load(Ordering::Relaxed);
        let now = Self::now_ms();
        let elapsed = Duration::from_millis(now.saturating_sub(last));
        elapsed > self.deadline
    }

    /// Run the watchdog loop. Cancels `cancel` and returns when the deadline
    /// expires without a `pet()`, triggering a safe-stop of the owning task.
    pub async fn run(&self, cancel: CancellationToken) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if self.is_expired() {
                        tracing::error!(
                            deadline_secs = self.deadline.as_secs(),
                            "command watchdog expired — triggering safe-stop"
                        );
                        cancel.cancel();
                        return;
                    }
                }
                () = cancel.cancelled() => return,
            }
        }
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "millis since epoch fits in u64 until year 584,942,417"
    )]
    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_not_expired_immediately() {
        let wd = CommandWatchdog::new(Duration::from_secs(5));
        assert!(!wd.is_expired());
    }

    #[test]
    fn watchdog_expired_after_deadline() {
        let wd = CommandWatchdog::new(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(10));
        assert!(wd.is_expired());
    }

    #[test]
    fn watchdog_reset_by_pet() {
        let wd = CommandWatchdog::new(Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(30));
        wd.pet();
        std::thread::sleep(Duration::from_millis(30));
        assert!(!wd.is_expired()); // 30ms since pet, deadline is 50ms
    }
}
