use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Monitors a heartbeat file's mtime to detect if the worker process is still alive.
pub struct HeartbeatMonitor {
    path: PathBuf,
    deadline: Duration,
}

impl HeartbeatMonitor {
    /// Create a new heartbeat monitor that watches the given file path.
    ///
    /// If the file's mtime is older than `deadline`, the process is considered dead.
    pub const fn new(path: PathBuf, deadline: Duration) -> Self {
        Self { path, deadline }
    }

    /// Returns the path being monitored.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Check if the heartbeat file exists and its mtime is within the deadline.
    pub fn is_alive(&self) -> bool {
        let Ok(metadata) = std::fs::metadata(&self.path) else {
            return false;
        };

        let Ok(modified) = metadata.modified() else {
            return false;
        };

        // SystemTimeError means the mtime is in the future -- still alive
        modified.elapsed().map_or(true, |elapsed| elapsed < self.deadline)
    }

    /// Update the heartbeat file's mtime (touch). Creates the file if it doesn't exist.
    pub fn touch(&self) -> io::Result<()> {
        std::fs::write(&self.path, b"")
    }
}

// ---------------------------------------------------------------------------
// HeartbeatTracker — in-memory, per-worker heartbeat tracking for NATS
// ---------------------------------------------------------------------------

/// Tracks the last heartbeat time for each known worker.
///
/// Workers that have not sent a heartbeat within `stale_threshold`
/// are reported as stale by [`stale_workers`](Self::stale_workers).
pub struct HeartbeatTracker {
    workers: HashMap<String, Instant>,
    stale_threshold: Duration,
}

impl HeartbeatTracker {
    /// Create a tracker with the given staleness threshold.
    pub fn new(stale_threshold: Duration) -> Self {
        Self {
            workers: HashMap::new(),
            stale_threshold,
        }
    }

    /// Record a heartbeat from `worker_id`, resetting its timer.
    pub fn record(&mut self, worker_id: &str) {
        self.workers.insert(worker_id.to_owned(), Instant::now());
    }

    /// Return the IDs of workers whose last heartbeat exceeds the threshold.
    pub fn stale_workers(&self) -> Vec<&str> {
        self.workers
            .iter()
            .filter(|(_, last_seen)| last_seen.elapsed() >= self.stale_threshold)
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// Stop tracking a worker (e.g. after issuing an e-stop).
    pub fn remove(&mut self, worker_id: &str) {
        self.workers.remove(worker_id);
    }

    /// Number of workers currently being tracked.
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn fresh_file_is_alive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("heartbeat");
        std::fs::write(&path, b"").unwrap();

        let monitor = HeartbeatMonitor::new(path, Duration::from_secs(10));
        assert!(monitor.is_alive(), "freshly created file should be alive");
    }

    #[test]
    fn file_older_than_deadline_is_not_alive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("heartbeat");
        std::fs::write(&path, b"").unwrap();

        // Use a very short deadline
        let monitor = HeartbeatMonitor::new(path, Duration::from_millis(50));

        // Wait past the deadline
        thread::sleep(Duration::from_millis(100));

        assert!(!monitor.is_alive(), "file older than deadline should not be alive");
    }

    #[test]
    fn missing_file_is_not_alive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent_heartbeat");

        let monitor = HeartbeatMonitor::new(path, Duration::from_secs(10));
        assert!(!monitor.is_alive(), "missing file should not be alive");
    }

    #[test]
    fn touch_updates_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("heartbeat");
        std::fs::write(&path, b"").unwrap();

        let monitor = HeartbeatMonitor::new(path, Duration::from_millis(100));

        // Wait almost to deadline
        thread::sleep(Duration::from_millis(60));

        // Touch should refresh the mtime
        monitor.touch().unwrap();

        // Should still be alive because we just touched it
        assert!(monitor.is_alive(), "touch should reset the mtime");
    }

    #[test]
    fn touch_creates_file_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new_heartbeat");

        let monitor = HeartbeatMonitor::new(path.clone(), Duration::from_secs(10));
        assert!(!path.exists());

        monitor.touch().unwrap();
        assert!(path.exists(), "touch should create the file");
        assert!(monitor.is_alive(), "newly touched file should be alive");
    }

    // -------------------------------------------------------------------
    // HeartbeatTracker tests
    // -------------------------------------------------------------------

    #[test]
    fn new_tracker_has_no_workers() {
        let tracker = HeartbeatTracker::new(Duration::from_secs(30));
        assert_eq!(tracker.worker_count(), 0);
        assert!(tracker.stale_workers().is_empty());
    }

    #[test]
    fn record_heartbeat_adds_worker() {
        let mut tracker = HeartbeatTracker::new(Duration::from_secs(30));
        tracker.record("worker-1");
        assert_eq!(tracker.worker_count(), 1);
        assert!(
            tracker.stale_workers().is_empty(),
            "just-recorded worker should not be stale"
        );
    }

    #[test]
    fn stale_worker_detected() {
        let mut tracker = HeartbeatTracker::new(Duration::from_millis(1));
        tracker.record("worker-1");

        // Wait past the threshold
        thread::sleep(Duration::from_millis(10));

        let stale = tracker.stale_workers();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0], "worker-1");
    }

    #[test]
    fn fresh_heartbeat_clears_staleness() {
        let mut tracker = HeartbeatTracker::new(Duration::from_millis(1));
        tracker.record("worker-1");

        thread::sleep(Duration::from_millis(10));
        assert!(!tracker.stale_workers().is_empty(), "should be stale before re-record");

        // Re-record to refresh the timestamp
        tracker.record("worker-1");
        assert!(
            tracker.stale_workers().is_empty(),
            "re-recorded worker should no longer be stale"
        );
        assert_eq!(tracker.worker_count(), 1, "should still be one worker, not duplicated");
    }

    #[test]
    fn remove_worker_stops_tracking() {
        let mut tracker = HeartbeatTracker::new(Duration::from_secs(30));
        tracker.record("worker-1");
        tracker.record("worker-2");
        assert_eq!(tracker.worker_count(), 2);

        tracker.remove("worker-1");
        assert_eq!(tracker.worker_count(), 1);
        assert!(tracker.stale_workers().is_empty());
    }

    #[test]
    fn remove_nonexistent_worker_is_noop() {
        let mut tracker = HeartbeatTracker::new(Duration::from_secs(30));
        tracker.remove("ghost");
        assert_eq!(tracker.worker_count(), 0);
    }

    #[test]
    fn multiple_workers_only_stale_ones_reported() {
        let mut tracker = HeartbeatTracker::new(Duration::from_millis(50));

        // Record an "old" worker
        tracker.record("old-worker");

        thread::sleep(Duration::from_millis(80));

        // Record a "fresh" worker after sleeping
        tracker.record("fresh-worker");

        let stale = tracker.stale_workers();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0], "old-worker");
    }
}
