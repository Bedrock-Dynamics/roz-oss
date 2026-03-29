use anyhow::Result;
use futures::StreamExt;
use roz_safety::estop::EStopEvent;
use roz_safety::heartbeat::HeartbeatTracker;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let logfire = logfire::configure()
        .with_service_name("roz-safety")
        .with_service_version(env!("CARGO_PKG_VERSION"))
        .with_environment(std::env::var("ROZ_ENVIRONMENT").unwrap_or_else(|_| "development".into()))
        .finish()
        .expect("failed to configure logfire");
    let _guard = logfire.shutdown_guard();

    let nats_url = std::env::var("ROZ_NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".into());
    tracing::info!(nats_url, "starting roz-safety daemon");

    let nats = async_nats::connect(&nats_url).await?;
    tracing::info!("connected to NATS");

    // Subscribe to all heartbeats
    let mut heartbeat_sub = nats.subscribe("events.*.heartbeat").await?;
    tracing::info!("monitoring worker heartbeats");

    // Subscribe to safety events
    let mut safety_sub = nats.subscribe("safety.>").await?;
    tracing::info!("monitoring safety events");

    // Per-worker heartbeat tracker: workers that miss 30s are stale
    let mut tracker = HeartbeatTracker::new(Duration::from_secs(30));

    // Watchdog loop: publish our own heartbeat and check for stale workers
    let watchdog_nats = nats.clone();
    let (stale_tx, mut stale_rx) = tokio::sync::mpsc::channel::<Vec<String>>(16);

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;

            // Publish the safety daemon's own watchdog heartbeat
            if let Err(e) = watchdog_nats
                .publish("safety.watchdog.heartbeat", bytes::Bytes::from_static(b"{}"))
                .await
            {
                tracing::error!(error = %e, "watchdog heartbeat failed");
            }
        }
    });

    loop {
        tokio::select! {
            Some(msg) = heartbeat_sub.next() => {
                // Subject format: events.{worker_id}.heartbeat
                let parts: Vec<&str> = msg.subject.as_str().split('.').collect();
                if parts.len() >= 2 {
                    let worker_id = parts[1];
                    tracker.record(worker_id);
                    tracing::trace!(worker_id, "worker heartbeat recorded");
                } else {
                    tracing::warn!(subject = %msg.subject, "malformed heartbeat subject");
                }

                // Check for stale workers after each heartbeat
                let stale = tracker.stale_workers();
                if !stale.is_empty() {
                    let stale_ids: Vec<String> = stale.iter().map(|s| (*s).to_owned()).collect();
                    // Send stale worker IDs to e-stop handler (non-blocking)
                    let _ = stale_tx.try_send(stale_ids);
                }
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
                    tracker.remove(worker_id);
                }
            }
            Some(msg) = safety_sub.next() => {
                let subject = msg.subject.as_str();

                // Skip our own watchdog heartbeat to avoid log noise
                if subject == "safety.watchdog.heartbeat" {
                    continue;
                }

                // Try to parse as a safety command for structured logging
                match roz_safety::commands::SafetyCommand::parse(&msg.payload) {
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
