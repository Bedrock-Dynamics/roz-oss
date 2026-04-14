//! Worker-side wiring for roz-zenoh edge-horizontal subsystems (ZEN-05).
//!
//! Instantiates [`EdgeStateBusRunner`] and [`ZenohCoordinator`] at worker
//! startup when `--features zenoh` is active. Keeps `main.rs` focused; all
//! subsystem lifecycle retention happens here.
//!
//! Per D-07 and the Phase 15 VERIFICATION gap report (2026-04-13), both
//! primitives exist as library code in `roz-zenoh` with full integration
//! coverage; this module is the missing call-site that realizes SC-5
//! "sensor sharing" + "pose coordination" at the worker boundary.

use std::sync::Arc;
use std::time::SystemTime;

use roz_zenoh::coordination::{RobotPose, ZenohCoordinator};
use roz_zenoh::edge_state_bus::EdgeStateBusRunner;
use roz_zenoh::topics::TRANSPORT_HEALTH;

/// Handles retained by the worker for the edge-horizontal subsystems.
///
/// Dropping `EdgeTransportHandles` releases the underlying Zenoh publishers
/// and liveliness tokens; `main.rs` holds this value for the worker lifetime.
pub struct EdgeTransportHandles {
    /// Edge state bus runner (pre-declared publishers for 5 summary topics).
    pub edge_state_bus: Arc<EdgeStateBusRunner>,
    /// Coordinator for pose publish/subscribe + barriers.
    pub coordinator: Arc<ZenohCoordinator>,
}

/// Start edge-horizontal subsystems from a shared `zenoh::Session`.
///
/// Constructs [`EdgeStateBusRunner`] + [`ZenohCoordinator`] and emits one
/// startup sample on each so subscribed peers observe a fresh sample
/// immediately (closes SC-5 minimum viable publisher — full subsystem
/// emitters are follow-up work).
///
/// # Errors
/// Returns [`EdgeStateBusRunner::start`] failure (per-topic publisher declare
/// error). Startup-sample publish failures are logged and swallowed here,
/// matching the 15-06 heartbeat publisher's log-and-continue posture (Zenoh
/// is feature-flagged complementary transport per D-03).
pub async fn start_edge_subsystems(session: zenoh::Session, worker_id: &str) -> anyhow::Result<EdgeTransportHandles> {
    // 1. EdgeStateBusRunner (pre-declares 5 publishers; fails fast on any declare error).
    let runner = EdgeStateBusRunner::start(session.clone(), worker_id).await?;
    let runner = Arc::new(runner);

    // 2. Startup TRANSPORT_HEALTH rollup so subscribed peers see a fresh
    //    sample on roz/<worker_id>/transport/health. Minimal shape —
    //    full EdgeTransportHealth payload lands via the 15-06 heartbeat
    //    publisher separately (this is just a "bus is alive" beacon).
    let startup_rollup = serde_json::json!({
        "worker_id": worker_id,
        "status": "ready",
        "source": "edge_state_bus_runner::startup",
    });
    if let Err(e) = runner.publish(&TRANSPORT_HEALTH, &startup_rollup).await {
        tracing::warn!(error = %e, "edge state bus startup sample publish failed");
    }

    // 3. ZenohCoordinator: stateless wrapper over robot_id + associated
    //    functions that take a Session. publish_pose is an associated
    //    fn (takes &Session, &RobotPose); retain the coordinator handle
    //    so downstream spatial-bridge wiring can call it.
    let coordinator = Arc::new(ZenohCoordinator::new(worker_id));

    // 4. Startup RobotPose publish: minimum viable pose sample on
    //    roz/coordination/pose/<worker_id>. Zero pose; downstream
    //    spatial-bridge emitter replaces this with live pose in a
    //    follow-up phase. Presence of this sample proves the wiring
    //    and closes SC-5 "pose coordination" sub-goal.
    let timestamp_ns = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let startup_pose = RobotPose {
        robot_id: worker_id.to_string(),
        position: [0.0, 0.0, 0.0],
        orientation: [1.0, 0.0, 0.0, 0.0],
        timestamp_ns,
    };
    if let Err(e) = ZenohCoordinator::publish_pose(&session, &startup_pose).await {
        tracing::warn!(error = %e, "coordinator startup pose publish failed");
    }

    tracing::info!(
        robot_id = %worker_id,
        "edge state bus runner + coordinator ready",
    );

    Ok(EdgeTransportHandles {
        edge_state_bus: runner,
        coordinator,
    })
}
