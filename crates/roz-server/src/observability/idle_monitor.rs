//! Phase 26 OBS-01 D-05: abandoned-session finalization helper.
//!
//! Exposes the idle-check tick interval and the env-driven idle timeout. The
//! per-session `WriterActor` owns the actual idle check: a `tokio::select!`
//! branch in its `run` loop fires every [`IDLE_CHECK_INTERVAL`] and
//! self-emits `WriteCommand::Finalize { IdleTimeout }` when
//! `last_message_at.elapsed() >= idle_timeout`. Keeping the check inside the
//! actor avoids shared-state plumbing (no `Arc<AtomicU64>` on `last_message_at`).
//!
//! The test contract is "a writer with no `WriteCommand::Event` for
//! `idle_timeout` seconds transitions to `status='finalized_idle_timeout'`."

use std::time::Duration;

/// Tick cadence for the idle check branch of the `WriterActor::run` select loop.
///
/// ~30 s is a compromise between responsiveness (upper bound on the delay
/// between crossing the idle threshold and finalize) and CPU cost (one cheap
/// `Instant::elapsed` per writer per tick). RESEARCH §Pattern 1 "Idle monitor
/// is a second tokio task that wakes every ~30 s".
pub const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// Resolve the effective idle timeout from `ROZ_MCAP_IDLE_TIMEOUT_SECS`.
///
/// Defaults to [`crate::observability::DEFAULT_MCAP_IDLE_TIMEOUT_SECS`] (600 s
/// per D-05). Parse failures fall back to the default (permissive — operators
/// can set the var to `0` if they want immediate finalize-on-idle semantics,
/// though that makes every 30 s tick close the writer).
#[must_use]
pub fn idle_timeout_from_env() -> Duration {
    let secs = std::env::var(crate::observability::ENV_MCAP_IDLE_TIMEOUT_SECS)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(crate::observability::DEFAULT_MCAP_IDLE_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

#[cfg(test)]
#[allow(
    unsafe_code,
    reason = "Edition-2024 std::env::{set_var,remove_var} are unsafe; env-var tests are serialized \
              by ENV_LOCK so we never observe torn writes from another thread."
)]
mod tests {
    use super::{IDLE_CHECK_INTERVAL, idle_timeout_from_env};
    use std::sync::Mutex;
    use std::time::Duration;

    // Guards env-var mutation in the two tests below (cargo test runs tests
    // in the same process; ROZ_MCAP_IDLE_TIMEOUT_SECS read/write races would
    // produce nondeterministic results without this lock).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn idle_check_interval_is_30_seconds() {
        assert_eq!(IDLE_CHECK_INTERVAL, Duration::from_secs(30));
    }

    #[test]
    fn idle_timeout_from_env_parses_override() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        // SAFETY: test-only env mutation — `set_var`/`remove_var` are unsafe
        // in Rust 2024 because concurrent reads from other threads could
        // observe torn writes; the ENV_LOCK serializes this against the
        // sibling test and no production reads occur during cargo test.
        unsafe {
            std::env::set_var(crate::observability::ENV_MCAP_IDLE_TIMEOUT_SECS, "42");
        }
        assert_eq!(idle_timeout_from_env(), Duration::from_secs(42));
        unsafe {
            std::env::remove_var(crate::observability::ENV_MCAP_IDLE_TIMEOUT_SECS);
        }
    }

    #[test]
    fn idle_timeout_from_env_default_is_600s() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            std::env::remove_var(crate::observability::ENV_MCAP_IDLE_TIMEOUT_SECS);
        }
        assert_eq!(idle_timeout_from_env(), Duration::from_secs(600));
    }
}
