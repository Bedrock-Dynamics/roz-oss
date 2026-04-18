use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

    // ===== Phase 24 FS-01 / D-02 extensions (RED — impl lands in the GREEN
    // commit) =====

    #[test]
    fn existing_watchdog_construction_compatible() {
        let wd = CommandWatchdog::new(Duration::from_secs(5));
        assert!(!wd.is_latched());
    }

    #[test]
    fn on_expire_callback_fires_after_deadline() {
        let fired = Arc::new(AtomicBool::new(false));
        let fired_clone = fired.clone();
        let cb: OnExpireCallback = Arc::new(move || {
            fired_clone.store(true, Ordering::Relaxed);
        });
        let wd = CommandWatchdog::with_on_expire(Duration::from_millis(1), cb);
        std::thread::sleep(Duration::from_millis(10));
        assert!(wd.is_expired());
        wd.latched.store(true, Ordering::Relaxed);
        (wd.on_expire)();
        assert!(fired.load(Ordering::Relaxed));
    }

    #[test]
    fn motion_latched_after_expire() {
        let wd = CommandWatchdog::new(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(10));
        wd.latched.store(true, Ordering::Relaxed);
        assert!(wd.is_latched());
    }

    #[test]
    fn pet_does_not_clear_latch() {
        let wd = CommandWatchdog::new(Duration::from_millis(50));
        wd.latched.store(true, Ordering::Relaxed);
        let before = wd.last_pet_ms.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(5));
        wd.pet();
        let after = wd.last_pet_ms.load(Ordering::Relaxed);
        assert!(wd.is_latched(), "latch must persist across pet");
        assert_eq!(before, after, "pet must be a no-op while latched");
    }

    #[test]
    fn clear_failsafe_clears_latch_and_rearms() {
        let wd = CommandWatchdog::new(Duration::from_millis(50));
        wd.latched.store(true, Ordering::Relaxed);
        assert!(wd.is_latched());
        wd.last_pet_ms.store(0, Ordering::Relaxed);
        assert!(wd.is_expired());
        wd.clear_failsafe();
        assert!(!wd.is_latched());
        assert!(!wd.is_expired(), "clear_failsafe must re-arm the deadline");
    }

    #[tokio::test]
    async fn run_fires_callback_and_exits_on_expire() {
        let fired = Arc::new(AtomicBool::new(false));
        let fired_clone = fired.clone();
        let cb: OnExpireCallback = Arc::new(move || fired_clone.store(true, Ordering::Relaxed));
        let wd = Arc::new(CommandWatchdog::with_on_expire(Duration::from_millis(1), cb));
        let cancel = CancellationToken::new();
        let wd2 = wd.clone();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move { wd2.run(cancel2).await });
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(fired.load(Ordering::Relaxed), "callback must have fired");
        assert!(wd.is_latched(), "motion must be latched");
        let _ = tokio::time::timeout(Duration::from_millis(200), handle).await;
    }
}
