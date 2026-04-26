//! MAVLink transport adapters (serial / UDP) on top of upstream
//! `mavlink::connect_async`.
//!
//! Provides a [`TransportHandle`] that owns the long-lived
//! `AsyncMavConnection` plus reader + writer tasks bridging upstream I/O into
//! async mpsc channels per Phase 22 D-03.
//!
//! Per 25-RESEARCH.md §Anti-Patterns, the writer side is a SINGLE long-lived
//! `tokio::spawn`-backed task fed by an `mpsc::Sender<MavMessage>` — NOT a
//! per-send `tokio::spawn`. This keeps `ActuatorSink::send` non-blocking while
//! preserving the single-owner connection invariant.
//!
//! # Deviations from 25-06-PLAN
//!
//! * **Rule 1 — upstream API drift:** plan uses `Option<SigningData>` for
//!   `setup_signing`; upstream `mavlink::AsyncMavConnection::setup_signing`
//!   takes `Option<SigningConfig>` in 0.17.1. Signature updated to match
//!   upstream.
//! * **Rule 3 — dependency not yet available:** plan imports
//!   `crate::signing::TransportKind`, but `signing.rs` is still a stub (plan
//!   25-05 populates it). A minimal [`TransportKind`] enum is defined here and
//!   is expected to move into `signing.rs` once 25-05 lands.
//! * **Phase 26.11 teardown hardening:** the transport now uses upstream
//!   `connect_async` (tokio-1 feature) so UDP receives are cancellable at test
//!   shutdown. The previous sync `recv()` + `block_in_place` bridge could leave
//!   blocking runtime threads alive and forced several tests to call
//!   `std::process::exit(0)`.

pub mod serial;
pub mod udp;

use std::sync::Arc;

use mavlink::common::MavMessage;
use mavlink::{AsyncMavConnection, MavHeader, SigningConfig};
use tokio::sync::mpsc;

/// Copper's MAVLink companion ID per DEEP-MAV §3 + 25-CONTEXT.md D-04.
///
/// See <https://mavlink.io/en/messages/common.html#MAV_COMPONENT>.
pub const MAV_COMP_ID_ONBOARD_COMPUTER: u8 = 195;

/// QGroundControl / Mission Planner companion ID. Used only by the
/// coexistence-test shim (plan 25-16); NEVER set for copper itself.
pub const MAV_COMP_ID_MISSIONPLANNER: u8 = 190;

/// FCU (autopilot) system ID per DEEP-MAV §3.
pub const MAV_FCU_SYSTEM_ID: u8 = 1;

/// FCU (autopilot) component ID per DEEP-MAV §3.
pub const MAV_FCU_COMPONENT_ID: u8 = 1;

/// Capacity for the outbound writer channel.
///
/// One tick at 100 Hz buffers 1 second of backpressure before
/// `ActuatorSink::send` errors with `TrySendError::Full` — Phase 22 D-03 tick
/// budget is 10 ms, so this is conservative.
pub const OUTBOUND_CHANNEL_CAPACITY: usize = 100;

/// Capacity for the inbound reader channel.
///
/// MAVLink SITL rates peak around 50 msg/s per stream; 256 gives ~5 s of
/// burst buffering before the reader task's `send` errors and drops frames.
pub const INBOUND_CHANNEL_CAPACITY: usize = 256;

/// Which physical transport a [`TransportHandle`] is attached to.
///
/// Consumed by the backend (plan 25-12) when resolving posture-dependent
/// behaviour (e.g. D-03 default-signing-on for UDP, off for serial).
///
/// Note: this enum will move to `crate::signing` once plan 25-05 populates
/// that module. Keep this stub in sync with the plan's expected variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// Direct serial link (e.g. Pixhawk TELEM2 over `/dev/ttyUSB0 @ 921600`).
    Serial,
    /// UDP link (e.g. `udpin:0.0.0.0:14540` for PX4 SITL offboard).
    Udp,
}

/// Owned handle returned by [`open_transport`] that the backend holds for the
/// life of the MAVLink session.
///
/// Dropping this value aborts both background tasks; no graceful flush of the
/// outbound channel is performed (Phase 22 D-03 tick budget has no room for
/// one).
pub struct TransportHandle {
    /// Outbound writer — `ActuatorSink::send` +
    /// `DiscreteCommandSink<FlightCommand>::send_command` post `MavMessage`
    /// values here. The writer task drains this and calls
    /// `conn.send(&header, &msg)`.
    pub outbound: mpsc::Sender<MavMessage>,
    /// Inbound reader — the backend's `SensorSource::try_recv` drains this.
    /// The reader task pushes every received `MavMessage` here (filtering is
    /// the backend's concern).
    pub inbound: mpsc::Receiver<MavMessage>,
    /// Kind of transport (for posture resolution and logging).
    pub transport_kind: TransportKind,
    reader: Option<tokio::task::JoinHandle<()>>,
    writer: Option<tokio::task::JoinHandle<()>>,
}

impl TransportHandle {
    /// Abort and await the reader/writer tasks.
    ///
    /// This is intentionally explicit for tests and orderly shutdown paths.
    /// Dropping the handle still aborts tasks as a fallback.
    pub async fn shutdown(mut self) {
        if let Some(reader) = self.reader.take() {
            reader.abort();
            let _ = reader.await;
        }
        if let Some(writer) = self.writer.take() {
            writer.abort();
            let _ = writer.await;
        }
        tracing::debug!("TransportHandle shutdown complete");
    }
}

impl Drop for TransportHandle {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        if let Some(writer) = self.writer.take() {
            writer.abort();
        }
        tracing::debug!("TransportHandle dropped — reader + writer tasks aborted");
    }
}

/// Open the MAVLink transport at `url` and spawn reader + writer tasks.
///
/// `url` examples:
/// * `"serial:/dev/ttyUSB0:921600"` — serial port (Pixhawk TELEM2).
/// * `"udpin:0.0.0.0:14540"` — UDP bound locally for PX4 SITL broadcast.
///
/// `signing` is attached via `conn.setup_signing(signing)` before the reader
/// starts draining frames — per D-14, signing is posture-gated by the caller
/// (see `crate::signing::build_signing_data`, plan 25-05).
///
/// `our_system_id` is the worker's MAVLink sysid (caller-chosen, typically the
/// host-id hash modulo 255 to avoid collisions). `our_component_id` is always
/// [`MAV_COMP_ID_ONBOARD_COMPUTER`] for copper; do not vary without reading
/// Pitfall 3 in 25-RESEARCH.md.
///
/// # Errors
///
/// Returns `Err` if `mavlink::connect_async` fails (serial port missing, UDP
/// bind conflict, etc.).
pub async fn open_transport(
    url: &str,
    transport_kind: TransportKind,
    signing: Option<SigningConfig>,
    our_system_id: u8,
    our_component_id: u8,
) -> anyhow::Result<TransportHandle> {
    tracing::info!(
        url,
        ?transport_kind,
        signing_on = signing.is_some(),
        "opening MAVLink transport"
    );
    let mut conn = mavlink::connect_async::<MavMessage>(url).await?;
    conn.set_protocol_version(mavlink::MavlinkVersion::V2);
    if let Some(cfg) = signing {
        conn.setup_signing(Some(cfg));
    }
    let conn: Arc<dyn AsyncMavConnection<MavMessage> + Send + Sync> = Arc::from(conn);

    let (out_tx, out_rx) = mpsc::channel::<MavMessage>(OUTBOUND_CHANNEL_CAPACITY);
    let (in_tx, in_rx) = mpsc::channel::<MavMessage>(INBOUND_CHANNEL_CAPACITY);

    let reader_conn = Arc::clone(&conn);
    let reader = tokio::spawn(async move {
        reader_loop(reader_conn, in_tx).await;
    });

    let writer_conn = Arc::clone(&conn);
    let writer = tokio::spawn(async move {
        writer_loop(writer_conn, out_rx, our_system_id, our_component_id).await;
    });

    Ok(TransportHandle {
        outbound: out_tx,
        inbound: in_rx,
        transport_kind,
        reader: Some(reader),
        writer: Some(writer),
    })
}

/// Reader task — drains `conn.recv()` into `tx` forever.
///
/// On error, logs and backs off 10 ms before retrying (matches `io_grpc.rs`
/// stream loop).
async fn reader_loop(conn: Arc<dyn AsyncMavConnection<MavMessage> + Send + Sync>, tx: mpsc::Sender<MavMessage>) {
    loop {
        match conn.recv().await {
            Ok((_header, msg)) => {
                if tx.send(msg).await.is_err() {
                    tracing::debug!("inbound channel closed; reader exiting");
                    return;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "MavConnection recv error");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
    }
}

/// Writer task — drains `rx` of outbound [`MavMessage`]s, wraps each in a
/// fresh [`MavHeader`] (sysid/compid from args; sequence incremented per
/// send), and calls `conn.send`.
///
/// Per Anti-Pattern guidance, this is a SINGLE long-lived task, not a
/// per-send spawn.
async fn writer_loop(
    conn: Arc<dyn AsyncMavConnection<MavMessage> + Send + Sync>,
    mut rx: mpsc::Receiver<MavMessage>,
    our_system_id: u8,
    our_component_id: u8,
) {
    let mut sequence: u8 = 0;
    while let Some(msg) = rx.recv().await {
        let header = MavHeader {
            system_id: our_system_id,
            component_id: our_component_id,
            sequence,
        };
        sequence = sequence.wrapping_add(1);
        match conn.send(&header, &msg).await {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "MavConnection send error");
            }
        }
    }
    tracing::debug!("outbound channel closed; writer exiting");
}

#[cfg(test)]
mod tests {
    use super::{
        INBOUND_CHANNEL_CAPACITY, MAV_COMP_ID_MISSIONPLANNER, MAV_COMP_ID_ONBOARD_COMPUTER, MAV_FCU_COMPONENT_ID,
        MAV_FCU_SYSTEM_ID, OUTBOUND_CHANNEL_CAPACITY,
    };

    #[test]
    fn comp_id_constants_match_mavlink_spec() {
        // DEEP-MAV §3 + MAVLink common.xml MAV_COMPONENT
        assert_eq!(MAV_COMP_ID_ONBOARD_COMPUTER, 195);
        assert_eq!(MAV_COMP_ID_MISSIONPLANNER, 190);
        assert_eq!(MAV_FCU_SYSTEM_ID, 1);
        assert_eq!(MAV_FCU_COMPONENT_ID, 1);
    }

    #[test]
    fn channel_capacities_are_sensible() {
        // 1 s of 100 Hz tick buffer outbound; 5 s of 50 msg/s burst inbound.
        assert!(OUTBOUND_CHANNEL_CAPACITY >= 100);
        assert!(INBOUND_CHANNEL_CAPACITY >= 250);
    }
}
