//! Library form of the roz-safety daemon main loop, parameterizable for tests.
//!
//! `crates/roz-safety/src/main.rs` reads env vars then delegates to
//! [`run_safety_daemon`]. Integration tests construct a [`SafetyDaemonConfig`]
//! with a small `t_stale` so they can verify the timer-driven stale scan
//! without waiting 30s of wall time.
//!
//! ## FW-05(b) — Codex M3 Option B
//!
//! Two parallel `tokio::spawn` tasks both share `Arc<tokio::sync::Mutex<HeartbeatTracker>>`:
//!
//! - **Heartbeat-receive task**: subscribes `events.*.heartbeat`, calls
//!   `tracker.record(worker_id)`. Does NOT scan for stale workers.
//! - **Stale-scan timer task**: every `scan_period`, calls the pure
//!   [`heartbeat::run_stale_scan`](crate::heartbeat::run_stale_scan) function
//!   with `Instant::now()` and dispatches stale IDs to the e-stop publisher.
//!
//! Splitting the responsibilities means stale detection fires on the timer
//! cadence regardless of whether new heartbeats arrive — closing the codex
//! review's gap at the original `crates/roz-safety/src/main.rs:53`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::StreamExt;
use tokio::sync::{Mutex, mpsc};

use crate::estop::EStopEvent;
use crate::heartbeat::{HeartbeatTracker, run_stale_scan};

/// Configuration for the safety daemon run-loop.
pub struct SafetyDaemonConfig {
    /// NATS URL to connect to.
    pub nats_url: String,
    /// Worker is considered stale after this duration without heartbeats.
    /// Production default: 30s. Test override: small (e.g. 200 ms) for fast tests.
    pub t_stale: Duration,
    /// Period for the stale-scan timer to wake up. Production default: 1s.
    /// MUST be much smaller than `t_stale` so detection latency is bounded.
    pub scan_period: Duration,
    /// Period for publishing the safety daemon's own watchdog heartbeat.
    pub watchdog_heartbeat_period: Duration,
}

impl Default for SafetyDaemonConfig {
    fn default() -> Self {
        Self {
            nats_url: "nats://localhost:4222".into(),
            t_stale: Duration::from_secs(30),
            scan_period: Duration::from_secs(1),
            watchdog_heartbeat_period: Duration::from_secs(10),
        }
    }
}

/// Run the safety daemon main loop.
///
/// Spawns three tokio tasks (watchdog heartbeat, stale-scan timer, heartbeat
/// receiver) and a foreground select-loop for e-stop publishing. The function
/// runs until all NATS subscriptions close.
///
/// # Errors
///
/// Returns an error if the NATS connection or subscriptions fail to set up.
#[allow(
    clippy::too_many_lines,
    reason = "binary main loop with multiple subsystems; factoring further would obscure flow"
)]
pub async fn run_safety_daemon(cfg: SafetyDaemonConfig) -> Result<()> {
    tracing::info!(nats_url = %cfg.nats_url, t_stale_ms = %cfg.t_stale.as_millis(), "starting roz-safety daemon");

    let nats = async_nats::connect(&cfg.nats_url).await?;
    tracing::info!("connected to NATS");

    // Subscribe to all heartbeats and safety events.
    let mut heartbeat_sub = nats.subscribe("events.*.heartbeat").await?;
    let mut safety_sub = nats.subscribe("safety.>").await?;
    nats.flush().await?;
    tracing::info!("monitoring worker heartbeats and safety events");

    // Shared per-worker heartbeat tracker. tokio::sync::Mutex is async-aware
    // so the stale-scan and heartbeat-receive tasks can lock without blocking
    // each other or the broker poll.
    let tracker = Arc::new(Mutex::new(HeartbeatTracker::new(cfg.t_stale)));

    // Channel from stale-scan task → e-stop publisher (in this fn's loop).
    let (stale_tx, mut stale_rx) = mpsc::channel::<Vec<String>>(64);

    // ---- Watchdog heartbeat task (publishes our own liveness) -------------
    let watchdog_nats = nats.clone();
    let watchdog_period = cfg.watchdog_heartbeat_period;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(watchdog_period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(e) = watchdog_nats
                .publish("safety.watchdog.heartbeat", bytes::Bytes::from_static(b"{}"))
                .await
            {
                tracing::error!(error = %e, "watchdog heartbeat failed");
            }
        }
    });

    // ---- Stale-scan timer task (FW-05b — Codex M3 Option B) ---------------
    //
    // Fires every `scan_period` regardless of whether new heartbeats arrive.
    // Calls the pure `run_stale_scan` fn with `Instant::now()`; the scanned
    // tracker shares state with the heartbeat-receive task via `Arc<Mutex>`.
    let stale_scan_tracker = Arc::clone(&tracker);
    let stale_scan_tx = stale_tx.clone();
    let stale_scan_threshold = cfg.t_stale;
    let scan_period = cfg.scan_period;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(scan_period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Discard the immediate first tick so we don't fire before any
        // heartbeat has had a chance to arrive.
        interval.tick().await;
        loop {
            interval.tick().await;
            let stale_ids: Vec<String> = {
                let t = stale_scan_tracker.lock().await;
                run_stale_scan(std::time::Instant::now(), stale_scan_threshold, &t)
            };
            if !stale_ids.is_empty() && stale_scan_tx.send(stale_ids).await.is_err() {
                tracing::warn!("stale_scan_tx receiver dropped; exiting stale-scan task");
                break;
            }
        }
    });

    // ---- Foreground loop: heartbeat receive + e-stop publish + safety log -
    loop {
        tokio::select! {
            Some(msg) = heartbeat_sub.next() => {
                let parts: Vec<&str> = msg.subject.as_str().split('.').collect();
                if parts.len() >= 2 {
                    let worker_id = parts[1];
                    let mut t = tracker.lock().await;
                    t.record(worker_id);
                    tracing::trace!(worker_id, "worker heartbeat recorded");
                } else {
                    tracing::warn!(subject = %msg.subject, "malformed heartbeat subject");
                }
                // NOTE: stale detection is owned by the timer task, not here.
            }
            Some(stale_ids) = stale_rx.recv() => {
                for worker_id in &stale_ids {
                    let event = EStopEvent::heartbeat_timeout(worker_id);
                    let subject = match roz_nats::subjects::Subjects::estop(worker_id) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!(worker_id, error = %e, "invalid worker_id for estop subject");
                            continue;
                        }
                    };
                    match event.to_json_bytes() {
                        Ok(payload) => {
                            if let Err(e) = nats.publish(subject, payload).await {
                                tracing::error!(worker_id, error = %e, "failed to publish e-stop");
                            } else {
                                tracing::warn!(worker_id, "e-stop issued: heartbeat timeout");
                            }
                        }
                        Err(e) => {
                            tracing::error!(worker_id, error = %e, "failed to serialize e-stop event");
                        }
                    }
                    // Remove from tracker so subsequent timer scans do not
                    // double-fire on the same worker.
                    let mut t = tracker.lock().await;
                    t.remove(worker_id);
                }
            }
            Some(msg) = safety_sub.next() => {
                let subject = msg.subject.as_str();

                // Skip our own watchdog heartbeat to avoid log noise.
                if subject == "safety.watchdog.heartbeat" {
                    continue;
                }

                match crate::commands::SafetyCommand::parse(&msg.payload) {
                    Ok(cmd) => {
                        tracing::info!(subject, ?cmd, "safety command received");
                    }
                    Err(_) => {
                        tracing::info!(
                            subject,
                            payload_len = msg.payload.len(),
                            "safety event received (non-command)"
                        );
                    }
                }
            }
            else => {
                tracing::warn!("all NATS subscriptions closed, safety daemon exiting");
                break;
            }
        }
    }

    Ok(())
}
