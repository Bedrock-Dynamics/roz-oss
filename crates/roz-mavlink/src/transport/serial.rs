//! Serial transport — Pixhawk TELEM2 over `/dev/ttyUSB0 @ 921600`.
//!
//! Per 25-CONTEXT.md D-03, serial links default to signing OFF (USB is the
//! trusted bootstrap channel per MAVLink v2 spec). Override via
//! `[mavlink.signing]` in `roz.toml`.
//!
//! This file is a thin URL-formatting helper over upstream
//! `mavlink::connect(..)` — the actual reader / writer task wiring lives in
//! `transport::open_transport` (parent module).

use mavlink::SigningConfig;

use super::{TransportHandle, TransportKind, open_transport};

/// Open a serial MAVLink transport on `path` at `baud` (typically 921600).
///
/// # Errors
///
/// Returns `Err` if the serial port cannot be opened or configured.
#[allow(
    clippy::unused_async,
    reason = "kept async to mirror future tokio-1 connect_async migration"
)]
pub async fn open_serial(
    path: &str,
    baud: u32,
    signing: Option<SigningConfig>,
    our_system_id: u8,
    our_component_id: u8,
) -> anyhow::Result<TransportHandle> {
    let url = format!("serial:{path}:{baud}");
    open_transport(&url, TransportKind::Serial, signing, our_system_id, our_component_id).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn serial_url_format_matches_upstream_convention() {
        // Upstream mavlink::connect expects "serial:/dev/ttyUSB0:921600" verbatim.
        let url = format!("serial:{}:{}", "/dev/ttyUSB0", 921_600);
        assert_eq!(url, "serial:/dev/ttyUSB0:921600");
    }
}
