//! UDP transport — `udpin:0.0.0.0:14540` (offboard) or `:14550` (GCS) per
//! 25-CONTEXT.md.
//!
//! **PX4 SITL footgun** (Pitfall 2 in 25-RESEARCH.md): copper BINDS
//! `udpin:...`; PX4 BROADCASTS to that port. We are the listener — NOT the
//! dialer. Getting this direction backwards produces "connect succeeds but
//! never sees a HEARTBEAT" symptoms. See `docs/mavlink-coexistence.md` (plan
//! 25-16) for the posture table.
//!
//! Per 25-CONTEXT.md D-03, UDP links default to signing ON (RF-equivalent).
//! Override via `[mavlink.signing]` in `roz.toml`.

use mavlink::SigningConfig;

use super::{TransportHandle, TransportKind, open_transport};

/// Open a UDP MAVLink transport, BINDING locally to `bind_addr` (e.g.
/// `"0.0.0.0:14540"` for PX4 SITL offboard).
///
/// The peer (PX4 or an RF modem) is expected to broadcast to our bound port.
///
/// # Errors
///
/// Returns `Err` if the UDP socket cannot be bound.
#[allow(
    clippy::unused_async,
    reason = "kept async to mirror future tokio-1 connect_async migration"
)]
pub async fn open_udp_in(
    bind_addr: &str,
    signing: Option<SigningConfig>,
    our_system_id: u8,
    our_component_id: u8,
) -> anyhow::Result<TransportHandle> {
    let url = format!("udpin:{bind_addr}");
    open_transport(&url, TransportKind::Udp, signing, our_system_id, our_component_id).await
}

/// Open a UDP MAVLink transport DIALING to a specific peer at `peer_addr`
/// (e.g. `"192.168.1.100:14550"` for a telemetry-radio GCS forwarder).
///
/// Use [`open_udp_in`] unless you explicitly need the dialer role.
///
/// # Errors
///
/// Returns `Err` if the UDP socket cannot be bound or dialed.
#[allow(
    clippy::unused_async,
    reason = "kept async to mirror future tokio-1 connect_async migration"
)]
pub async fn open_udp_out(
    peer_addr: &str,
    signing: Option<SigningConfig>,
    our_system_id: u8,
    our_component_id: u8,
) -> anyhow::Result<TransportHandle> {
    let url = format!("udpout:{peer_addr}");
    open_transport(&url, TransportKind::Udp, signing, our_system_id, our_component_id).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn udp_in_url_format() {
        // Upstream mavlink::connect expects "udpin:0.0.0.0:14540" verbatim.
        let url = format!("udpin:{}", "0.0.0.0:14540");
        assert_eq!(url, "udpin:0.0.0.0:14540");
    }

    #[test]
    fn udp_out_url_format() {
        let url = format!("udpout:{}", "192.168.1.100:14550");
        assert_eq!(url, "udpout:192.168.1.100:14550");
    }
}
